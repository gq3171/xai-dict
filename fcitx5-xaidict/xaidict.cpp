/*
 * fcitx5-xaidict — real fcitx5 Input Method + DBus bridge for xai-dict.
 *
 * As Input Method ("语音听写"):
 *   - Appears in fcitx5 IM list; switch to it like Pinyin.
 *   - Super+V or F9 toggles recording via xai-dict Unix socket.
 *   - Shows status in preedit panel (idle / recording).
 *
 * As DBus bridge (used by xai-dict daemon for commit/preedit):
 *   path:  /xaidict
 *   iface: org.fcitx.Fcitx.XaiDict1
 *   methods: Commit(s)->b, Preedit(s)->b, ClearPreedit()->b,
 *            Toggle()->b, Status()->s, IsRecording()->b
 *
 * SPDX-License-Identifier: LGPL-2.1-or-later
 */

#include <chrono>
#include <cstring>
#include <fstream>
#include <memory>
#include <string>
#include <unistd.h>
#include <sys/socket.h>
#include <sys/un.h>

#include <fcitx-utils/dbus/message.h>
#include <fcitx-utils/dbus/objectvtable.h>
#include <fcitx-utils/i18n.h>
#include <fcitx-utils/key.h>
#include <fcitx-utils/keysym.h>
#include <fcitx-utils/log.h>
#include <fcitx-utils/textformatflags.h>
#include <fcitx/addonfactory.h>
#include <fcitx/addoninstance.h>
#include <fcitx/addonmanager.h>
#include <fcitx/event.h>
#include <fcitx/inputcontext.h>
#include <fcitx/inputmethodengine.h>
#include <fcitx/inputmethodentry.h>
#include <fcitx/inputpanel.h>
#include <fcitx/instance.h>
#include <fcitx/text.h>
#include <fcitx/userinterface.h>

#include <dbus_public.h>

namespace fcitx {

static constexpr char kIface[] = "org.fcitx.Fcitx.XaiDict1";
static constexpr char kPath[] = "/xaidict";

// ---------------------------------------------------------------------------
// Daemon socket helper
// ---------------------------------------------------------------------------

static std::string socketPath() {
    const char *runtime = getenv("XDG_RUNTIME_DIR");
    if (runtime && *runtime) {
        return std::string(runtime) + "/xai-dict.sock";
    }
    return "/tmp/xai-dict-" + std::to_string(getuid()) + ".sock";
}

/// Send one line command, return first reply line (empty on failure).
static std::string daemonCmd(const std::string &cmd) {
    const auto path = socketPath();
    int fd = ::socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (fd < 0) {
        return {};
    }
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    if (path.size() >= sizeof(addr.sun_path)) {
        ::close(fd);
        return {};
    }
    std::strncpy(addr.sun_path, path.c_str(), sizeof(addr.sun_path) - 1);
    if (::connect(fd, reinterpret_cast<sockaddr *>(&addr), sizeof(addr)) < 0) {
        ::close(fd);
        return {};
    }
    const std::string msg = cmd + "\n";
    if (::write(fd, msg.data(), msg.size()) < 0) {
        ::close(fd);
        return {};
    }
    // half-close write so daemon can finish
    ::shutdown(fd, SHUT_WR);

    char buf[512];
    std::string out;
    while (true) {
        const ssize_t n = ::read(fd, buf, sizeof(buf));
        if (n <= 0) {
            break;
        }
        out.append(buf, static_cast<size_t>(n));
        if (out.find('\n') != std::string::npos) {
            break;
        }
    }
    ::close(fd);
    while (!out.empty() && (out.back() == '\n' || out.back() == '\r')) {
        out.pop_back();
    }
    return out;
}

static bool replyIsRecording(const std::string &reply) {
    // "OK recording" / "OK recording\n"
    return reply.find("recording") != std::string::npos;
}

static bool isUsableTarget(InputContext *ic) {
    if (!ic) {
        return false;
    }
    const auto &prog = ic->program();
    if (prog == "python3" || prog == "python" || prog == "osd_bar.py" ||
        prog.find("xai-dict") != std::string::npos ||
        prog.find("settings_gui") != std::string::npos) {
        return false;
    }
    return true;
}

static InputContext *focusedUsable(Instance *inst) {
    InputContext *found = nullptr;
    inst->inputContextManager().foreachFocused([&](InputContext *ic) {
        if (ic && ic->hasFocus() && isUsableTarget(ic)) {
            found = ic;
            return false;
        }
        return true;
    });
    return found;
}

// ---------------------------------------------------------------------------
// DBus service
// ---------------------------------------------------------------------------

class XaiDictEngine;

class XaiDictService : public dbus::ObjectVTable<XaiDictService> {
public:
    explicit XaiDictService(XaiDictEngine *parent) : parent_(parent) {}

    bool commit(const std::string &text);
    bool preedit(const std::string &text);
    bool clearPreedit();
    bool toggle();
    std::string status();
    bool isRecording();

private:
    FCITX_OBJECT_VTABLE_METHOD(commit, "Commit", "s", "b");
    FCITX_OBJECT_VTABLE_METHOD(preedit, "Preedit", "s", "b");
    FCITX_OBJECT_VTABLE_METHOD(clearPreedit, "ClearPreedit", "", "b");
    FCITX_OBJECT_VTABLE_METHOD(toggle, "Toggle", "", "b");
    FCITX_OBJECT_VTABLE_METHOD(status, "Status", "", "s");
    FCITX_OBJECT_VTABLE_METHOD(isRecording, "IsRecording", "", "b");

    XaiDictEngine *parent_;
};

// ---------------------------------------------------------------------------
// Input Method Engine
// ---------------------------------------------------------------------------

class XaiDictEngine : public InputMethodEngine {
public:
    explicit XaiDictEngine(Instance *instance);
    ~XaiDictEngine() override = default;

    Instance *instance() { return instance_; }

    std::vector<InputMethodEntry> listInputMethods() override {
        std::vector<InputMethodEntry> entries;
        entries.emplace_back(std::move(
            InputMethodEntry("xai-dict", "Voice Dictation", "zh_CN", "xaidict")
                .setNativeName("语音听写")
                .setIcon("audio-input-microphone")
                .setLabel("🎤")
                .setConfigurable(false)));
        return entries;
    }

    void keyEvent(const InputMethodEntry &entry, KeyEvent &event) override;
    void activate(const InputMethodEntry &entry,
                  InputContextEvent &event) override;
    void deactivate(const InputMethodEntry &entry,
                    InputContextEvent &event) override;
    void reset(const InputMethodEntry &entry,
               InputContextEvent &event) override;
    std::string subMode(const InputMethodEntry &, InputContext &) override;

    // Shared helpers used by DBus
    bool doCommit(const std::string &text);
    bool doPreedit(const std::string &text);
    bool doClearPreedit();
    bool doToggle();
    std::string doStatus();
    bool recording() const { return recording_; }
    void setRecording(bool v);
    void updateStatusPanel(InputContext *ic);

private:
    FCITX_ADDON_DEPENDENCY_LOADER(dbus, instance_->addonManager());

    bool isToggleKey(const Key &key) const;
    void showHint(InputContext *ic, const std::string &msg,
                  bool underline = false);

    Instance *instance_;
    dbus::Bus *bus_ = nullptr;
    std::unique_ptr<XaiDictService> service_;
    bool recording_ = false;
    /// Last preedit from streaming ASR (not the status line).
    std::string livePreedit_;
};

XaiDictEngine::XaiDictEngine(Instance *instance) : instance_(instance) {
    bus_ = dbus()->call<IDBusModule::bus>();
    service_ = std::make_unique<XaiDictService>(this);
    bus_->addObjectVTable(kPath, kIface, *service_);
    bus_->flush();
    FCITX_INFO() << "xai-dict: InputMethod + DBus bridge ready at " << kPath;
}

bool XaiDictEngine::isToggleKey(const Key &key) const {
    // Super+V  or  F9  or  bare Super_R (less conflict than Alt_R with daemon)
    if (key.check(FcitxKey_v, KeyState::Super) ||
        key.check(FcitxKey_V, KeyState::Super)) {
        return true;
    }
    if (key.check(FcitxKey_F9)) {
        return true;
    }
    // Also allow Right Alt when this IM is active (daemon should set hotkey=none
    // to avoid double-toggle via evdev).
    if (key.check(FcitxKey_Alt_R) || key.check(FcitxKey_ISO_Level3_Shift)) {
        return true;
    }
    return false;
}

void XaiDictEngine::setRecording(bool v) {
    recording_ = v;
    if (!v) {
        livePreedit_.clear();
    }
}

void XaiDictEngine::showHint(InputContext *ic, const std::string &msg,
                             bool underline) {
    if (!ic) {
        return;
    }
    Text t;
    if (!msg.empty()) {
        t.append(msg, underline ? TextFormatFlag::Underline
                                : TextFormatFlag::HighLight);
    }
    // Server preedit (kimpanel / apps without client preedit)
    ic->inputPanel().setPreedit(t);
    // Client preedit when available
    ic->inputPanel().setClientPreedit(t);
    ic->updatePreedit();
    ic->updateUserInterface(UserInterfaceComponent::InputPanel);
}

void XaiDictEngine::updateStatusPanel(InputContext *ic) {
    if (!ic) {
        ic = focusedUsable(instance_);
    }
    if (!ic) {
        return;
    }
    if (recording_) {
        if (!livePreedit_.empty()) {
            showHint(ic, livePreedit_, true);
        } else {
            showHint(ic, "🎤 录音中… Super+V / F9 结束", false);
        }
    } else {
        showHint(ic, "🎤 语音听写就绪 · Super+V 或 F9 开始", false);
    }
}

std::string XaiDictEngine::subMode(const InputMethodEntry &, InputContext &) {
    return recording_ ? "录音中" : "就绪";
}

void XaiDictEngine::activate(const InputMethodEntry &,
                             InputContextEvent &event) {
    // Sync status with daemon
    const auto st = doStatus();
    setRecording(replyIsRecording(st));
    updateStatusPanel(event.inputContext());
    FCITX_INFO() << "xai-dict: IM activated, daemon=" << st;
}

void XaiDictEngine::deactivate(const InputMethodEntry &entry,
                               InputContextEvent &event) {
    // Don't stop recording on switch-away — user may still want background
    // dictation via daemon hotkey. Only clear panel.
    if (auto *ic = event.inputContext()) {
        ic->inputPanel().reset();
        ic->updatePreedit();
        ic->updateUserInterface(UserInterfaceComponent::InputPanel);
    }
    InputMethodEngine::deactivate(entry, event);
}

void XaiDictEngine::reset(const InputMethodEntry &, InputContextEvent &event) {
    livePreedit_.clear();
    if (auto *ic = event.inputContext()) {
        ic->inputPanel().reset();
        ic->updatePreedit();
    }
}

void XaiDictEngine::keyEvent(const InputMethodEntry &, KeyEvent &event) {
    // Only react on key press
    if (event.isRelease()) {
        return;
    }
    if (!isToggleKey(event.key())) {
        // Absorb most keys while recording so they don't leak into the field
        // as partial typing — except Escape which cancels.
        if (recording_) {
            if (event.key().check(FcitxKey_Escape)) {
                // Stop if recording
                doToggle();
                event.filterAndAccept();
                return;
            }
            // Let modifiers through, block printable chars while recording
            if (!event.key().states().test(KeyState::Ctrl) &&
                !event.key().states().test(KeyState::Super) &&
                event.key().isSimple()) {
                event.filterAndAccept();
                return;
            }
        }
        return;
    }

    event.filterAndAccept();
    doToggle();
    if (auto *ic = event.inputContext()) {
        updateStatusPanel(ic);
    }
}

bool XaiDictEngine::doToggle() {
    const auto reply = daemonCmd("TOGGLE");
    if (reply.empty()) {
        FCITX_INFO() << "xai-dict: daemon not reachable (" << socketPath()
                     << ")";
        if (auto *ic = focusedUsable(instance_)) {
            showHint(ic, "⚠ xai-dict daemon 未运行", false);
        }
        return false;
    }
    // Brief wait so STATUS reflects post-toggle state.
    usleep(80 * 1000);
    const auto st = daemonCmd("STATUS");
    bool now = !recording_;
    if (!st.empty()) {
        now = replyIsRecording(st);
    } else if (reply.find("recording") != std::string::npos) {
        now = true;
    } else if (reply.find("idle") != std::string::npos) {
        now = false;
    }
    setRecording(now);
    FCITX_INFO() << "xai-dict: toggle reply='" << reply << "' status='" << st
                 << "' recording=" << recording_;
    updateStatusPanel(focusedUsable(instance_));
    return true;
}

std::string XaiDictEngine::doStatus() {
    auto st = daemonCmd("STATUS");
    if (st.empty()) {
        return "ERR offline";
    }
    setRecording(replyIsRecording(st));
    return st;
}

bool XaiDictEngine::doCommit(const std::string &text) {
    if (text.empty()) {
        return false;
    }
    auto *ic = focusedUsable(instance_);
    if (!ic) {
        FCITX_INFO() << "xai-dict: Commit — no focused IC";
        return false;
    }
    // Clear preedit then commit
    ic->inputPanel().setClientPreedit(Text());
    ic->inputPanel().setPreedit(Text());
    ic->updatePreedit();
    ic->commitString(text);
    livePreedit_.clear();
    // Stay in "recording" if stream mode is mid-session; refresh status
    const auto st = daemonCmd("STATUS");
    if (!st.empty()) {
        setRecording(replyIsRecording(st));
    }
    if (recording_) {
        updateStatusPanel(ic);
    } else {
        // Brief idle hint
        showHint(ic, "🎤 语音听写就绪 · Super+V 或 F9 开始", false);
    }
    FCITX_INFO() << "xai-dict: commitString program=" << ic->program()
                 << " bytes=" << text.size();
    return true;
}

bool XaiDictEngine::doPreedit(const std::string &text) {
    auto *ic = focusedUsable(instance_);
    if (!ic) {
        return false;
    }
    livePreedit_ = text;
    recording_ = true; // receiving partials implies session active
    Text t;
    if (!text.empty()) {
        t.append(text, TextFormatFlag::Underline);
    } else {
        t.append("🎤 录音中…", TextFormatFlag::HighLight);
    }
    ic->inputPanel().setClientPreedit(t);
    ic->inputPanel().setPreedit(t);
    ic->updatePreedit();
    ic->updateUserInterface(UserInterfaceComponent::InputPanel);
    return true;
}

bool XaiDictEngine::doClearPreedit() {
    auto *ic = focusedUsable(instance_);
    livePreedit_.clear();
    if (!ic) {
        return false;
    }
    ic->inputPanel().setClientPreedit(Text());
    ic->inputPanel().setPreedit(Text());
    ic->updatePreedit();
    ic->updateUserInterface(UserInterfaceComponent::InputPanel);
    return true;
}

// ---------------------------------------------------------------------------
// DBus bindings
// ---------------------------------------------------------------------------

bool XaiDictService::commit(const std::string &text) {
    return parent_->doCommit(text);
}
bool XaiDictService::preedit(const std::string &text) {
    return parent_->doPreedit(text);
}
bool XaiDictService::clearPreedit() { return parent_->doClearPreedit(); }
bool XaiDictService::toggle() { return parent_->doToggle(); }
std::string XaiDictService::status() { return parent_->doStatus(); }
bool XaiDictService::isRecording() {
    parent_->doStatus();
    return parent_->recording();
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

class XaiDictFactory : public AddonFactory {
public:
    AddonInstance *create(AddonManager *manager) override {
        return new XaiDictEngine(manager->instance());
    }
};

} // namespace fcitx

FCITX_ADDON_FACTORY_V2(xaidict, fcitx::XaiDictFactory);
