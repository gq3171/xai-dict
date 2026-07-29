/*
 * fcitx5-xaidict — Module for xai-dict voice dictation.
 *
 * Design goal: work **alongside** Pinyin (or any other IM).
 *   - Does NOT steal the current input method.
 *   - Super+V / F9 globally toggles recording (while you stay on 拼音).
 *   - Daemon still commits/preedits via DBus into the focused field.
 *
 * DBus (org.fcitx.Fcitx5 /xaidict org.fcitx.Fcitx.XaiDict1):
 *   Commit(s)->b  Preedit(s)->b  ClearPreedit()->b
 *   Toggle()->b   Status()->s    IsRecording()->b
 *
 * SPDX-License-Identifier: LGPL-2.1-or-later
 */

#include <cstring>
#include <memory>
#include <string>
#include <unistd.h>
#include <sys/socket.h>
#include <sys/un.h>

#include <fcitx-utils/dbus/objectvtable.h>
#include <fcitx-utils/key.h>
#include <fcitx-utils/keysym.h>
#include <fcitx-utils/log.h>
#include <fcitx-utils/textformatflags.h>
#include <fcitx/addonfactory.h>
#include <fcitx/addoninstance.h>
#include <fcitx/addonmanager.h>
#include <fcitx/event.h>
#include <fcitx/inputcontext.h>
#include <fcitx/inputpanel.h>
#include <fcitx/instance.h>
#include <fcitx/text.h>
#include <fcitx/userinterface.h>

#include <dbus_public.h>

namespace fcitx {

static constexpr char kIface[] = "org.fcitx.Fcitx.XaiDict1";
static constexpr char kPath[] = "/xaidict";

// ---------------------------------------------------------------------------
// Daemon socket
// ---------------------------------------------------------------------------

static std::string socketPath() {
    const char *runtime = getenv("XDG_RUNTIME_DIR");
    if (runtime && *runtime) {
        return std::string(runtime) + "/xai-dict.sock";
    }
    return "/tmp/xai-dict-" + std::to_string(getuid()) + ".sock";
}

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
// Forward
// ---------------------------------------------------------------------------

class XaiDictModule;

class XaiDictService : public dbus::ObjectVTable<XaiDictService> {
public:
    explicit XaiDictService(XaiDictModule *parent) : parent_(parent) {}

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

    XaiDictModule *parent_;
};

// ---------------------------------------------------------------------------
// Module (always on, works with Pinyin)
// ---------------------------------------------------------------------------

class XaiDictModule : public AddonInstance {
public:
    explicit XaiDictModule(Instance *instance);
    ~XaiDictModule() override = default;

    Instance *instance() { return instance_; }

    bool doCommit(const std::string &text);
    bool doPreedit(const std::string &text);
    bool doClearPreedit();
    bool doToggle();
    std::string doStatus();
    bool recording() const { return recording_; }
    void setRecording(bool v) {
        recording_ = v;
        if (!v) {
            livePreedit_.clear();
        }
    }

private:
    FCITX_ADDON_DEPENDENCY_LOADER(dbus, instance_->addonManager());

    bool isToggleKey(const Key &key) const;
    void flashHint(InputContext *ic, const std::string &msg, bool underline);

    Instance *instance_;
    dbus::Bus *bus_ = nullptr;
    std::unique_ptr<XaiDictService> service_;
    std::unique_ptr<HandlerTableEntry<EventHandler>> keyHandler_;
    bool recording_ = false;
    std::string livePreedit_;
};

XaiDictModule::XaiDictModule(Instance *instance) : instance_(instance) {
    bus_ = dbus()->call<IDBusModule::bus>();
    service_ = std::make_unique<XaiDictService>(this);
    bus_->addObjectVTable(kPath, kIface, *service_);
    bus_->flush();

    // After Pinyin handles keys: intercept only our hotkeys.
    // Other keys pass through to 拼音 / keyboard unchanged.
    keyHandler_ = instance_->watchEvent(
        EventType::InputContextKeyEvent, EventWatcherPhase::PostInputMethod,
        [this](Event &event) {
            auto &keyEvent = static_cast<KeyEvent &>(event);
            if (keyEvent.filtered() || keyEvent.isRelease()) {
                return;
            }
            if (!isToggleKey(keyEvent.key())) {
                return;
            }
            keyEvent.filterAndAccept();
            doToggle();
        });

    FCITX_INFO() << "xai-dict: module ready (global Super+V / F9; DBus "
                 << kPath << ")";
}

bool XaiDictModule::isToggleKey(const Key &key) const {
    // Super+V  (primary, does not leave 拼音)
    if (key.check(FcitxKey_v, KeyState::Super) ||
        key.check(FcitxKey_V, KeyState::Super)) {
        return true;
    }
    // F9
    if (key.check(FcitxKey_F9)) {
        return true;
    }
    // Do NOT intercept Right Alt / AltGr (ISO_Level3_Shift):
    //  - breaks layouts that type symbols via AltGr
    //  - double-fires with daemon hotkey=rightalt (toggle → start+stop)
    // Use Super+V / F9 here; Right Alt stays on the daemon evdev path.
    return false;
}

void XaiDictModule::flashHint(InputContext *ic, const std::string &msg,
                              bool underline) {
    if (!ic) {
        return;
    }
    Text t;
    if (!msg.empty()) {
        t.append(msg, underline ? TextFormatFlag::Underline
                                : TextFormatFlag::HighLight);
    }
    ic->inputPanel().setClientPreedit(t);
    ic->inputPanel().setPreedit(t);
    ic->updatePreedit();
    ic->updateUserInterface(UserInterfaceComponent::InputPanel);
}

bool XaiDictModule::doToggle() {
    const auto reply = daemonCmd("TOGGLE");
    if (reply.empty()) {
        FCITX_INFO() << "xai-dict: daemon offline (" << socketPath() << ")";
        if (auto *ic = focusedUsable(instance_)) {
            flashHint(ic, "⚠ xai-dict daemon 未运行", false);
        }
        return false;
    }
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
    FCITX_INFO() << "xai-dict: toggle → recording=" << recording_ << " ("
                 << reply << " / " << st << ")";

    if (auto *ic = focusedUsable(instance_)) {
        if (recording_) {
            flashHint(ic, "🎤 录音中… Super+V / F9 结束", false);
        } else {
            // Clear status preedit so 拼音 can show candidates again
            doClearPreedit();
        }
    }
    return true;
}

std::string XaiDictModule::doStatus() {
    auto st = daemonCmd("STATUS");
    if (st.empty()) {
        return "ERR offline";
    }
    setRecording(replyIsRecording(st));
    return st;
}

bool XaiDictModule::doCommit(const std::string &text) {
    if (text.empty()) {
        return false;
    }
    auto *ic = focusedUsable(instance_);
    if (!ic) {
        FCITX_INFO() << "xai-dict: Commit — no focused IC";
        return false;
    }
    ic->inputPanel().setClientPreedit(Text());
    ic->inputPanel().setPreedit(Text());
    ic->updatePreedit();
    ic->commitString(text);
    livePreedit_.clear();

    // Keep recording flag in sync (stream mode may still be on)
    const auto st = daemonCmd("STATUS");
    if (!st.empty()) {
        setRecording(replyIsRecording(st));
    }
    if (recording_) {
        flashHint(ic, "🎤 录音中… Super+V / F9 结束", false);
    }
    FCITX_INFO() << "xai-dict: commit program=" << ic->program()
                 << " n=" << text.size();
    return true;
}

bool XaiDictModule::doPreedit(const std::string &text) {
    auto *ic = focusedUsable(instance_);
    if (!ic) {
        return false;
    }
    livePreedit_ = text;
    recording_ = true;
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

bool XaiDictModule::doClearPreedit() {
    livePreedit_.clear();
    auto *ic = focusedUsable(instance_);
    if (!ic) {
        return false;
    }
    ic->inputPanel().setClientPreedit(Text());
    ic->inputPanel().setPreedit(Text());
    ic->updatePreedit();
    ic->updateUserInterface(UserInterfaceComponent::InputPanel);
    return true;
}

// DBus
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

class XaiDictFactory : public AddonFactory {
public:
    AddonInstance *create(AddonManager *manager) override {
        return new XaiDictModule(manager->instance());
    }
};

} // namespace fcitx

FCITX_ADDON_FACTORY_V2(xaidict, fcitx::XaiDictFactory);
