#include "pengu.h"
#include "v8_wrapper.h"

static V8Value *v8_open_devtools(V8Value *const *args, int argc)
{
    auto context = cef_v8context_get_current_context();
    auto frame = context->get_frame(context);

    // IPC to browser process.
    auto msg = cef_process_message_create(&u"@open-devtools"_s);
    frame->send_process_message(frame, PID_BROWSER, msg);

    return nullptr;
}

V8HandlerFunctionEntry v8_HelperEntries[]
{
    { "OpenDevTools", v8_open_devtools },
    { nullptr },
};
