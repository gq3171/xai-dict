/*
 * fcitx5-xaidict — bridge for xai-dict voice dictation.
 *
 * DBus (on org.fcitx.Fcitx5):
 *   path:  /xaidict
 *   iface: org.fcitx.Fcitx.XaiDict1
 *   method Commit(s)  -> b   final text into focused field
 *   method Preedit(s) -> b   live partial (underlined / client preedit)
 *   method ClearPreedit() -> b
 *
 * SPDX-License-Identifier: LGPL-2.1-or-later
 */

#include <memory>
#include <string>

#include <fcitx-utils/dbus/message.h>
#include <fcitx-utils/dbus/objectvtable.h>
#include <fcitx-utils/log.h>
#include <fcitx-utils/textformatflags.h>
#include <fcitx/addonfactory.h>
#include <fcitx/addoninstance.h>
#include <fcitx/addonmanager.h>
#include <fcitx/inputcontext.h>
#include <fcitx/inputpanel.h>
#include <fcitx/instance.h>
#include <fcitx/text.h>

#include <dbus_public.h>

namespace fcitx {

static constexpr char kIface[] = "org.fcitx.Fcitx.XaiDict1";
static constexpr char kPath[] = "/xaidict";

class XaiDictModule;

class XaiDictService : public dbus::ObjectVTable<XaiDictService> {
public:
    explicit XaiDictService(XaiDictModule *parent) : parent_(parent) {}

    bool commit(const std::string &text);
    bool preedit(const std::string &text);
    bool clearPreedit();

private:
    FCITX_OBJECT_VTABLE_METHOD(commit, "Commit", "s", "b");
    FCITX_OBJECT_VTABLE_METHOD(preedit, "Preedit", "s", "b");
    FCITX_OBJECT_VTABLE_METHOD(clearPreedit, "ClearPreedit", "", "b");
    XaiDictModule *parent_;
};

class XaiDictModule : public AddonInstance {
public:
    explicit XaiDictModule(Instance *instance);
    ~XaiDictModule() override = default;

    Instance *instance() { return instance_; }

private:
    FCITX_ADDON_DEPENDENCY_LOADER(dbus, instance_->addonManager());

    Instance *instance_;
    dbus::Bus *bus_ = nullptr;
    std::unique_ptr<XaiDictService> service_;
};

XaiDictModule::XaiDictModule(Instance *instance) : instance_(instance) {
    bus_ = dbus()->call<IDBusModule::bus>();
    service_ = std::make_unique<XaiDictService>(this);
    bus_->addObjectVTable(kPath, kIface, *service_);
    bus_->flush();
    FCITX_INFO() << "xai-dict: DBus bridge ready at " << kPath;
}

static bool isUsableTarget(InputContext *ic) {
    if (!ic) {
        return false;
    }
    const auto &prog = ic->program();
    if (prog == "python3" || prog == "python" || prog == "osd_bar.py" ||
        prog.find("xai-dict") != std::string::npos) {
        return false;
    }
    return true;
}

static InputContext *focusedUsable(Instance *inst) {
    InputContext *found = nullptr;
    inst->inputContextManager().foreachFocused([&](InputContext *ic) {
        if (ic && ic->hasFocus() && isUsableTarget(ic)) {
            found = ic;
            return false; // stop
        }
        return true;
    });
    return found;
}

bool XaiDictService::commit(const std::string &text) {
    if (text.empty()) {
        return false;
    }
    auto *ic = focusedUsable(parent_->instance());
    if (!ic) {
        FCITX_INFO() << "xai-dict: Commit — no focused IC";
        return false;
    }
    // Clear any live preedit first.
    ic->inputPanel().setClientPreedit(Text());
    ic->updatePreedit();
    ic->commitString(text);
    FCITX_INFO() << "xai-dict: commitString program=" << ic->program()
                 << " bytes=" << text.size();
    return true;
}

bool XaiDictService::preedit(const std::string &text) {
    auto *ic = focusedUsable(parent_->instance());
    if (!ic) {
        return false;
    }
    Text t;
    if (!text.empty()) {
        // Underline-ish hint for client-side preedit when supported.
        t.append(text, TextFormatFlag::Underline);
    }
    ic->inputPanel().setClientPreedit(t);
    ic->updatePreedit();
    return true;
}

bool XaiDictService::clearPreedit() {
    auto *ic = focusedUsable(parent_->instance());
    if (!ic) {
        return false;
    }
    ic->inputPanel().setClientPreedit(Text());
    ic->updatePreedit();
    return true;
}

class XaiDictFactory : public AddonFactory {
public:
    AddonInstance *create(AddonManager *manager) override {
        return new XaiDictModule(manager->instance());
    }
};

} // namespace fcitx

FCITX_ADDON_FACTORY_V2(xaidict, fcitx::XaiDictFactory);
