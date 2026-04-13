#include "browser.h"
#include <cstring>
#include <filesystem>
#include <shlobj.h>
#include <string>
#include "include/capi/cef_resource_handler_capi.h"
#include "include/capi/cef_scheme_capi.h"
#include "include/capi/cef_stream_capi.h"

// BROWSER PROCESS ONLY.
//
// Serves the `https://talon/*` scheme to the LoL client's renderer.
// Running as a custom CEF scheme rather than a localhost HTTP server
// gives us three things:
//
//  1. Same-origin fetches from `https://riot:*/` (both are HTTPS), so
//     no mixed-content or CORS headaches.
//  2. No ports to bind, no port collisions with other injection tools.
//  3. Image responses can stream PNG bytes straight from disk without
//     base64-embedding into JSON.
//
// Routes:
//   GET /skins/*   → stream `<appdata>/com.talon.app/skins_index.json`
//   GET /assets/*  → reserved for skin preview images (not yet wired)
//
// The index file is written by Talon's Rust backend whenever the
// enabled-skin state changes. We don't parse JSON in C++; we just
// stream the file's bytes as `application/json` and let `preload.js`
// filter by championId client-side.

static std::wstring get_skins_index_path()
{
    WCHAR appdata[MAX_PATH];
    HRESULT hr = SHGetFolderPathW(nullptr, CSIDL_APPDATA, nullptr, 0, appdata);
    if (FAILED(hr))
        return L"";
    return std::wstring(appdata) + L"\\com.talon.app\\skins_index.json";
}

static std::u16string wide_to_u16(const std::wstring &ws)
{
    // On Windows, wchar_t and char16_t are both 16-bit UTF-16 code
    // units, so a reinterpret-copy preserves the string contents
    // without any allocation-heavy conversion.
    return std::u16string(
        reinterpret_cast<const char16_t *>(ws.c_str()),
        ws.length());
}

class TalonResourceHandler : public CefRefCount<cef_resource_handler_t>
{
public:
    TalonResourceHandler()
        : CefRefCount(this)
        , stream_(nullptr)
        , length_(0)
    {
        cef_bind_method(TalonResourceHandler, open);
        cef_bind_method(TalonResourceHandler, get_response_headers);
        cef_bind_method(TalonResourceHandler, skip);
        cef_bind_method(TalonResourceHandler, read);
    }

    ~TalonResourceHandler()
    {
        if (stream_ != nullptr)
            stream_->base.release(&stream_->base);
    }

private:
    cef_stream_reader_t *stream_;
    int64 length_;
    std::u16string mime_;
    // Kept alive for the duration of the handler so CefStr::wrap's
    // non-owning pointer into its buffer stays valid until CEF has
    // consumed the file name.
    std::u16string file_path_storage_;

    int _open(cef_request_t *request, int *handle_request, cef_callback_t *callback)
    {
        CefScopedStr url = request->get_url(request);
        std::u16string full((char16_t *)url.str, url.length);

        // Strip "https://talon" prefix (13 chars) to get the path.
        std::u16string path = (full.length() > 13) ? full.substr(13) : std::u16string();

        // Trim query string.
        size_t q = path.find(u'?');
        if (q != std::u16string::npos)
            path = path.substr(0, q);

        if (path.rfind(u"/skins/", 0) == 0)
        {
            std::wstring wpath = get_skins_index_path();
            if (!wpath.empty())
            {
                std::filesystem::path fp(wpath);
                if (std::filesystem::is_regular_file(fp))
                {
                    file_path_storage_ = wide_to_u16(wpath);
                    stream_ = cef_stream_reader_create_for_file(
                        &CefStr::wrap(file_path_storage_));
                    if (stream_ != nullptr)
                    {
                        stream_->seek(stream_, 0, SEEK_END);
                        length_ = stream_->tell(stream_);
                        stream_->seek(stream_, 0, SEEK_SET);
                        mime_ = u"application/json";
                        *handle_request = 1;
                        return 1;
                    }
                }
            }

            // Index file missing or unreadable → serve an empty object
            // so preload.js gets a well-formed JSON response.
            static const char EMPTY_INDEX[] = "{}";
            stream_ = cef_stream_reader_create_for_data(
                (void *)EMPTY_INDEX, sizeof(EMPTY_INDEX) - 1);
            length_ = sizeof(EMPTY_INDEX) - 1;
            mime_ = u"application/json";
        }
        else
        {
            stream_ = nullptr;
        }

        *handle_request = 1;
        return 1;
    }

    void _get_response_headers(cef_response_t *response, int64 *response_length, cef_string_t *redirect_url)
    {
        response->set_header_by_name(response, &u"Access-Control-Allow-Origin"_s, &u"*"_s, 1);
        response->set_header_by_name(response, &u"Cache-Control"_s, &u"no-store"_s, 1);

        if (stream_ == nullptr)
        {
            response->set_status(response, 404);
            response->set_error(response, ERR_FILE_NOT_FOUND);
            *response_length = -1;
            return;
        }

        response->set_status(response, 200);
        response->set_error(response, ERR_NONE);
        if (!mime_.empty())
            response->set_mime_type(response, &CefStr::wrap(mime_));
        *response_length = length_;
    }

    int _skip(int64 bytes_to_skip, int64 *bytes_skipped, cef_resource_skip_callback_t *callback)
    {
        if (stream_ == nullptr)
        {
            *bytes_skipped = -2;
            return 0;
        }
        stream_->seek(stream_, bytes_to_skip, SEEK_CUR);
        *bytes_skipped = bytes_to_skip;
        return 1;
    }

    int _read(void *data_out, int bytes_to_read, int *bytes_read, cef_resource_read_callback_t *callback)
    {
        *bytes_read = 0;
        if (stream_ == nullptr)
            return 0;

        int read = static_cast<int>(stream_->read(stream_, data_out, 1, bytes_to_read));
        *bytes_read = read;
        return (*bytes_read > 0);
    }
};

struct TalonSchemeHandlerFactory : CefRefCount<cef_scheme_handler_factory_t>
{
    TalonSchemeHandlerFactory() : CefRefCount(this)
    {
        cef_scheme_handler_factory_t::create = create;
    }

    static cef_resource_handler_t *CEF_CALLBACK create(
        struct _cef_scheme_handler_factory_t *self,
        struct _cef_browser_t *browser,
        struct _cef_frame_t *frame,
        const cef_string_t *scheme_name,
        struct _cef_request_t *request)
    {
        return new TalonResourceHandler();
    }
};

void browser::register_talon_domain(cef_request_context_t *ctx)
{
    auto scheme = u"https"_s;
    auto domain = u"talon"_s;
    auto factory = new TalonSchemeHandlerFactory();

    ctx->register_scheme_handler_factory(ctx, &scheme, &domain, factory);
}
