#pragma once
#include "pengu.h"
#include "include/capi/cef_browser_capi.h"
#include "include/capi/cef_frame_capi.h"
#include "include/capi/cef_request_context_capi.h"

namespace browser
{
    void register_plugins_domain(cef_request_context_t *ctx);
    void register_talon_domain(cef_request_context_t *ctx);
}
