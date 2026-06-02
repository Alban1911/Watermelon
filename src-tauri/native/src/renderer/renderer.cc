#include "pengu.h"
#include "hook.h"
#include "v8_wrapper.h"
#include <unordered_map>
#include "include/capi/cef_app_capi.h"
#include "include/capi/cef_render_process_handler_capi.h"

// RENDERER PROCESS ONLY.

static bool is_main_ = false;

extern V8HandlerFunctionEntry v8_HelperEntries[];

static std::vector<path> get_plugin_entries()
{
    std::vector<path> entries;
    auto plugins_dir = config::plugins_dir();

    if (file::is_dir(plugins_dir))
    {
        for (const auto &name : file::read_dir(plugins_dir))
        {
            auto ch1 = name.c_str()[0];

            if (ch1 == '_' || ch1 == '.')
                continue;

            auto fpath = plugins_dir / name;

            if (file::is_file(fpath))
            {
                if (name.string().ends_with(".js"))
                {
                    entries.push_back(name);
                }
            }
            else if (file::is_dir(fpath))
            {
                if (ch1 == '@')
                {
                    for (const auto &subname : file::read_dir(fpath))
                    {
                        auto sub_ch1 = subname.c_str()[0];
                        if (sub_ch1 == '_' || sub_ch1 == '.')
                            continue;

                        if (file::is_file(fpath / subname / "index.js"))
                        {
                            entries.push_back(name / subname / "index.js");
                        }
                    }
                }
                else if (file::is_file(fpath / "index.js"))
                {
                    entries.push_back(name / "index.js");
                }
            }
        }
    }

    return entries;
}

struct NativeV8Handler : CefRefCount<cef_v8handler_t>
{
    std::unordered_map<std::string, V8FunctionHandler> map_;

    NativeV8Handler() : CefRefCount(this)
    {
        cef_bind_method(NativeV8Handler, execute);
    }

private:
    int CALLBACK _execute(
        const cef_string_t* name,
        cef_v8value_t* object,
        size_t _argc,
        cef_v8value_t* const* _args,
        cef_v8value_t** retval,
        cef_string_t* exception)
    {
        cef_string_utf8_t func{""};
        cef_string_to_utf8(name->str, name->length, &func);

        bool handled = false;
        auto it = map_.find(func.str);

        if (it != map_.end())
        {
            int argc = static_cast<int>(_argc);
            auto args = reinterpret_cast<V8Value *const *>(_args);

            auto result = it->second(args, argc);
            if (result != nullptr)
                *retval = reinterpret_cast<cef_v8value_t *>(result);

            handled = true;
        }

        cef_string_utf8_clear(&func);
        return handled;
    }
};

static void ExposeNativeFunctions(V8Object *window)
{
    auto native = V8Object::create();
    auto handler = new NativeV8Handler();

    auto list = {
        v8_HelperEntries,
    };

    for (auto &entries : list) {
        for (auto entry = entries; entry->name; entry++) {
            handler->map_[entry->name] = entry->func;
            auto name = CefStr(entry->name);
            auto function = V8Value::function(&name, handler);
            native->set(&name, function, V8_PROPERTY_ATTRIBUTE_READONLY);
        }
    }

    window->set(&u"__native"_s, native, V8_PROPERTY_ATTRIBUTE_READONLY);
}

static void LoadPlugins(V8Object *window)
{
    auto pengu = V8Object::create();

    // Pengu.version
    auto version = V8Value::string(&CefStr(""));
    pengu->set(&u"version"_s, version, V8_PROPERTY_ATTRIBUTE_NONE);

    // Pengu.superPotato
    auto superPotato = V8Value::boolean(config::options::super_potato());
    pengu->set(&u"superPotato"_s, superPotato, V8_PROPERTY_ATTRIBUTE_READONLY);

    pengu->set(&u"isMac"_s, V8Value::boolean(false), V8_PROPERTY_ATTRIBUTE_READONLY);

    // Pengu.plugins
    auto entries = get_plugin_entries();
    auto pluginEntries = V8Array::create((int)entries.size());

    for (int index = 0; index < (int)entries.size(); index++)
    {
        auto entry = CefStr::from_path(entries[index]);
        auto value = V8Value::string(&entry);
        pluginEntries->set(index, value);
    }

    pengu->set(&u"plugins"_s, pluginEntries, V8_PROPERTY_ATTRIBUTE_READONLY);

    // Pengu.disabledPlugins
    auto disabledPlugins = CefStr(config::disabled_plugins());
    pengu->set(&u"disabledPlugins"_s, V8Value::string(&disabledPlugins), V8_PROPERTY_ATTRIBUTE_NONE);

    window->set(&u"Pengu"_s, pengu, V8_PROPERTY_ATTRIBUTE_READONLY);
}

static void ExecutePreloadScript(cef_frame_t *frame)
{
    void *buffer; size_t length;
    path preload_path = config::loader_dir() / "plugins" / "preload.js";

    if (file::read_file(preload_path, &buffer, &length))
    {
        CefStr script((const char *)buffer, length);
        frame->execute_java_script(frame, &script, &u"https://plugins/@/preload"_s, 1);
        free(buffer);
    }
    else
    {
        logutil::write("[Watermelon] preload.js not found");
    }
}

static decltype(cef_render_process_handler_t::on_context_created) OnContextCreated;
static void CEF_CALLBACK Hooked_OnContextCreated(
    struct _cef_render_process_handler_t* self,
    struct _cef_browser_t* browser,
    struct _cef_frame_t* frame,
    struct _cef_v8context_t* context)
{
    CefScopedStr url = frame->get_url(frame);

    // Detect main page.
    if (is_main_ && url.startw("https://riot:") && url.endw("/index.html"))
    {
        logutil::write("[Watermelon] V8 context created for main page");

        auto window = context->get_global(context);

        ExposeNativeFunctions(reinterpret_cast<V8Object *>(window));
        LoadPlugins(reinterpret_cast<V8Object *>(window));
        ExecutePreloadScript(frame);
    }

    OnContextCreated(self, browser, frame, context);
}

static decltype(cef_render_process_handler_t::on_browser_created) OnBrowserCreated;
static void CEF_CALLBACK Hooked_OnBrowserCreated(
    struct _cef_render_process_handler_t* self,
    struct _cef_browser_t* browser,
    struct _cef_dictionary_value_t* extra_info)
{
    is_main_ = extra_info && extra_info->has_key(extra_info, &u"is_main"_s);

    OnBrowserCreated(self, browser, extra_info);
}

static hook::Hook<decltype(&cef_execute_process)> CefExecuteProcess;
static int Hooked_CefExecuteProcess(const cef_main_args_t* args, cef_app_t* app, void* windows_sandbox_info)
{
    static auto Old_GetRenderProcessHandler = app->get_render_process_handler;
    app->get_render_process_handler = [](cef_app_t* self) -> cef_render_process_handler_t*
    {
        auto handler = Old_GetRenderProcessHandler(self);

        OnContextCreated = handler->on_context_created;
        handler->on_context_created = Hooked_OnContextCreated;

        OnBrowserCreated = handler->on_browser_created;
        handler->on_browser_created = Hooked_OnBrowserCreated;

        return handler;
    };

    return CefExecuteProcess(args, app, windows_sandbox_info);
}

void HookRendererProcess()
{
    logutil::write("[Watermelon] HookRendererProcess");
    CefExecuteProcess.hook(LIBCEF_MODULE_NAME, "cef_execute_process", Hooked_CefExecuteProcess);
}

