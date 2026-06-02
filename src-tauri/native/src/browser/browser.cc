#include "browser.h"
#include "hook.h"
#include "include/capi/cef_app_capi.h"
#include "include/capi/cef_client_capi.h"
#include "include/capi/cef_browser_capi.h"
#include "include/capi/cef_keyboard_handler_capi.h"
#include "include/internal/cef_types.h"

// BROWSER PROCESS ONLY.

static hook::Hook<decltype(&cef_request_context_create_context)> CefRequestContext_CreateContext;
static cef_request_context_t *Hooked_CefRequestContext_CreateContext(
    const struct _cef_request_context_settings_t *settings,
    struct _cef_request_context_handler_t *handler)
{
    const_cast<cef_request_context_settings_t *>(settings)->cache_path
        = CefStr::from_path(config::cache_dir()).forward();

    auto ctx = CefRequestContext_CreateContext(settings, handler);

    browser::register_plugins_domain(ctx);
    browser::register_talon_domain(ctx);

    return ctx;
}

// Toggle DevTools on a browser.
static void ToggleDevTools(cef_browser_t *browser)
{
    auto host = browser->get_host(browser);
    if (host->has_dev_tools(host))
        host->close_dev_tools(host);
    else
        host->show_dev_tools(host, nullptr, nullptr, nullptr, nullptr);
    host->base.release(&host->base);
}

// --- IPC message handler (handles @open-devtools from renderer) ---

static decltype(cef_client_t::on_process_message_received) Original_OnProcessMessageReceived;

static int CEF_CALLBACK Hooked_OnProcessMessageReceived(
    struct _cef_client_t* self,
    struct _cef_browser_t* browser,
    struct _cef_frame_t* frame,
    cef_process_id_t source_process,
    struct _cef_process_message_t* message)
{
    CefScopedStr name = message->get_name(message);
    if (name.equal("@open-devtools"))
    {
        ToggleDevTools(browser);
        return 1;
    }

    if (Original_OnProcessMessageReceived)
        return Original_OnProcessMessageReceived(self, browser, frame, source_process, message);
    return 0;
}

// --- Keyboard handler (F11 toggles DevTools in debug builds) ---

#ifndef NDEBUG
static decltype(cef_client_t::get_keyboard_handler) Original_GetKeyboardHandler;
static cef_keyboard_handler_t *g_original_keyboard_handler = nullptr;

static int CEF_CALLBACK OnPreKeyEvent(
    struct _cef_keyboard_handler_t* self,
    struct _cef_browser_t* browser,
    const cef_key_event_t* event,
    cef_event_handle_t os_event,
    int* is_keyboard_shortcut)
{
    // VK_F11 = 0x7A
    if (event->type == KEYEVENT_RAWKEYDOWN && event->windows_key_code == 0x7A)
    {
        ToggleDevTools(browser);
        return 1;
    }

    // Delegate to the original handler.
    if (g_original_keyboard_handler && g_original_keyboard_handler->on_pre_key_event)
        return g_original_keyboard_handler->on_pre_key_event(
            g_original_keyboard_handler, browser, event, os_event, is_keyboard_shortcut);

    return 0;
}

static int CEF_CALLBACK OnKeyEvent(
    struct _cef_keyboard_handler_t* self,
    struct _cef_browser_t* browser,
    const cef_key_event_t* event,
    cef_event_handle_t os_event)
{
    if (g_original_keyboard_handler && g_original_keyboard_handler->on_key_event)
        return g_original_keyboard_handler->on_key_event(
            g_original_keyboard_handler, browser, event, os_event);

    return 0;
}

static cef_keyboard_handler_t g_keyboard_handler = {};
static bool g_keyboard_handler_init = false;

static cef_keyboard_handler_t* CEF_CALLBACK Hooked_GetKeyboardHandler(
    struct _cef_client_t* self)
{
    if (!g_keyboard_handler_init)
    {
        if (Original_GetKeyboardHandler)
            g_original_keyboard_handler = Original_GetKeyboardHandler(self);

        g_keyboard_handler.base.size = sizeof(cef_keyboard_handler_t);
        g_keyboard_handler.on_pre_key_event = OnPreKeyEvent;
        g_keyboard_handler.on_key_event = OnKeyEvent;
        g_keyboard_handler_init = true;
    }
    return &g_keyboard_handler;
}
#endif // NDEBUG

// --- Browser creation hook ---

static hook::Hook<decltype(&cef_browser_host_create_browser)> CefBrowserHost_CreateBrowser;
static int Hooked_CefBrowserHost_CreateBrowser(
    const cef_window_info_t* windowInfo,
    struct _cef_client_t* client,
    const cef_string_t* url,
    const struct _cef_browser_settings_t* settings,
    struct _cef_dictionary_value_t* extra_info,
    struct _cef_request_context_t* request_context)
{
    auto &url_ = CefStr::borrow(url);

    // Hook main browser only.
    if (url_.startw("https://riot:") && url_.endw("/bootstrap.html"))
    {
        // Create extra info if null.
        if (extra_info == nullptr)
            extra_info = cef_dictionary_value_create();

        // Set as main browser.
        extra_info->set_null(extra_info, &u"is_main"_s);

        // Hook IPC message handler.
        Original_OnProcessMessageReceived = client->on_process_message_received;
        client->on_process_message_received = Hooked_OnProcessMessageReceived;

#ifndef NDEBUG
        // Hook keyboard handler for F11 DevTools toggle.
        Original_GetKeyboardHandler = client->get_keyboard_handler;
        client->get_keyboard_handler = Hooked_GetKeyboardHandler;
#endif
    }

    return CefBrowserHost_CreateBrowser(windowInfo, client, url, settings, extra_info, request_context);
}

static decltype(cef_app_t::on_before_command_line_processing) OnBeforeCommandLineProcessing;
static void CEF_CALLBACK Hooked_OnBeforeCommandLineProcessing(
    struct _cef_app_t* self,
    const cef_string_t* process_type,
    struct _cef_command_line_t* command_line)
{
    command_line->base.add_ref(&command_line->base);
    OnBeforeCommandLineProcessing(self, process_type, command_line);

    int rdport = config::options::debug_port();
    if (rdport > 0 && rdport < UINT16_MAX)
    {
        command_line->append_switch_with_value(command_line,
            &u"remote-debugging-port"_s, &CefStr(std::to_string(rdport)));
    }

    if (config::options::isecure_mode())
    {
        command_line->append_switch(command_line, &u"disable-web-security"_s);
    }

    if (config::options::optimized_client())
    {
        command_line->append_switch(command_line, &u"disable-background-timer-throttling"_s);
        command_line->append_switch(command_line, &u"disable-backgrounding-occluded-windows"_s);
        command_line->append_switch(command_line, &u"disable-renderer-backgrounding"_s);
        command_line->append_switch(command_line, &u"disable-metrics"_s);
        command_line->append_switch(command_line, &u"disable-component-update"_s);
        command_line->append_switch(command_line, &u"disable-domain-reliability"_s);
        command_line->append_switch(command_line, &u"disable-translate"_s);
        command_line->append_switch(command_line, &u"disable-gpu-watchdog"_s);
        command_line->append_switch(command_line, &u"disable-renderer-accessibility"_s);
        command_line->append_switch(command_line, &u"no-sandbox"_s);
    }

    if (config::options::super_potato())
    {
        command_line->append_switch(command_line, &u"disable-smooth-scrolling"_s);
        command_line->append_switch(command_line, &u"wm-window-animations-disabled"_s);
        command_line->append_switch_with_value(command_line, &u"animation-duration-scale"_s, &u"0"_s);
    }

    command_line->base.release(&command_line->base);
}

static hook::Hook<decltype(&cef_initialize)> CefInitialize;
static int Hooked_CefInitialize(const struct _cef_main_args_t* args,
    const struct _cef_settings_t* settings, cef_app_t* app, void* windows_sandbox_info)
{
    // Hook command line.
    OnBeforeCommandLineProcessing = app->on_before_command_line_processing;
    app->on_before_command_line_processing = Hooked_OnBeforeCommandLineProcessing;

    const_cast<cef_settings_t *>(settings)->cache_path
        = CefStr::from_path(config::cache_dir()).forward();

    const_cast<cef_settings_t *>(settings)->root_cache_path
        = CefStr::from_path(config::cache_dir()).forward();

    return CefInitialize(args, settings, app, windows_sandbox_info);
}

void HookBrowserProcess()
{
    OutputDebugStringA("[Watermelon] HookBrowserProcess");

    // Hook CefInitialize().
    CefInitialize.hook(LIBCEF_MODULE_NAME,
        "cef_initialize", Hooked_CefInitialize);

    // Hook CefBrowserHost::CreateBrowser().
    CefBrowserHost_CreateBrowser.hook(LIBCEF_MODULE_NAME,
        "cef_browser_host_create_browser", Hooked_CefBrowserHost_CreateBrowser);

    // Hook CefRequestContext::CreateContext().
    CefRequestContext_CreateContext.hook(LIBCEF_MODULE_NAME,
        "cef_request_context_create_context", Hooked_CefRequestContext_CreateContext);

    OutputDebugStringA("[Watermelon] Browser hooks installed");
}
