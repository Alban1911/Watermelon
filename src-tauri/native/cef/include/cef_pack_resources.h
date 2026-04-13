#ifndef __CEF_INCLUDE_CEF_PACK_RESOURCES_H_
#define __CEF_INCLUDE_CEF_PACK_RESOURCES_H_
#pragma once

#if defined(OS_WIN)
#include "include/cef_pack_resources_win.h"
#elif defined(OS_MAC)
#include "include/cef_pack_resources_mac.h"
#endif

#endif
