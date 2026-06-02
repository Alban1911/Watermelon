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
// Serves the `https://watermelon/*` scheme to the LoL client's renderer.
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
//   GET /skins/*             -> stream `<appdata>/Watermelon/library/skins_index.json`
//   GET /assets/background/* -> stream `<appdata>/Watermelon/cache/previews/background/*.png`
//   GET /assets/splash/*     -> stream `<appdata>/Watermelon/cache/previews/splash/*.png`
//   GET /assets/tile/*       -> stream `<appdata>/Watermelon/cache/previews/tile/*.png`
//
// The index file is written by Watermelon's Rust backend whenever the
// enabled-skin state changes. We don't parse JSON in C++; we just
// stream the file's bytes as `application/json` and let `preload.js`
// filter by championId client-side.

static std::wstring get_app_data_root()
{
    WCHAR appdata[MAX_PATH];
    HRESULT hr = SHGetFolderPathW(nullptr, CSIDL_APPDATA, nullptr, 0, appdata);
    if (FAILED(hr))
        return L"";
    return std::wstring(appdata) + L"\\Watermelon";
}

static std::wstring get_skins_index_path()
{
    std::wstring root = get_app_data_root();
    if (root.empty())
        return L"";
    return root + L"\\library\\skins_index.json";
}

static std::string get_skins_index_version()
{
    std::filesystem::path path(get_skins_index_path());
    std::error_code ec;
    auto exists = std::filesystem::exists(path, ec);
    if (ec || !exists)
        return "0";

    auto mtime = std::filesystem::last_write_time(path, ec);
    if (ec)
        return "0";

    return std::to_string(mtime.time_since_epoch().count());
}

static std::wstring get_previews_dir()
{
    std::wstring root = get_app_data_root();
    if (root.empty())
        return L"";
    return root + L"\\cache\\previews\\splash";
}

static std::wstring get_background_previews_dir()
{
    std::wstring root = get_app_data_root();
    if (root.empty())
        return L"";
    return root + L"\\cache\\previews\\background";
}

static std::wstring get_tile_previews_dir()
{
    std::wstring root = get_app_data_root();
    if (root.empty())
        return L"";
    return root + L"\\cache\\previews\\tile";
}

static std::wstring get_custom_background_previews_dir()
{
    std::wstring root = get_app_data_root();
    if (root.empty())
        return L"";
    return root + L"\\user-assets\\backgrounds";
}

static std::wstring get_custom_tile_previews_dir()
{
    std::wstring root = get_app_data_root();
    if (root.empty())
        return L"";
    return root + L"\\user-assets\\tiles";
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

static int hex_value(char16_t c)
{
    if (c >= u'0' && c <= u'9')
        return c - u'0';
    if (c >= u'a' && c <= u'f')
        return 10 + (c - u'a');
    if (c >= u'A' && c <= u'F')
        return 10 + (c - u'A');
    return -1;
}

static bool url_decode_path_component(const std::u16string &input, std::wstring *output)
{
    if (output == nullptr)
        return false;

    output->clear();
    output->reserve(input.size());

    for (size_t i = 0; i < input.size(); ++i)
    {
        char16_t c = input[i];
        if (c == u'%')
        {
            if (i + 2 >= input.size())
                return false;
            int hi = hex_value(input[i + 1]);
            int lo = hex_value(input[i + 2]);
            if (hi < 0 || lo < 0)
                return false;
            output->push_back(static_cast<wchar_t>((hi << 4) | lo));
            i += 2;
            continue;
        }
        if (c == u'+')
        {
            output->push_back(L' ');
            continue;
        }
        output->push_back(static_cast<wchar_t>(c));
    }

    return true;
}

class WatermelonResourceHandler : public CefRefCount<cef_resource_handler_t>
{
public:
    WatermelonResourceHandler()
        : CefRefCount(this)
        , stream_(nullptr)
        , length_(0)
    {
        cef_bind_method(WatermelonResourceHandler, open);
        cef_bind_method(WatermelonResourceHandler, get_response_headers);
        cef_bind_method(WatermelonResourceHandler, skip);
        cef_bind_method(WatermelonResourceHandler, read);
    }

    ~WatermelonResourceHandler()
    {
        if (stream_ != nullptr)
            stream_->base.release(&stream_->base);
    }

private:
    cef_stream_reader_t *stream_;
    int64 length_;
    std::u16string mime_;
    std::string body_storage_;
    // Kept alive for the duration of the handler so CefStr::wrap's
    // non-owning pointer into its buffer stays valid until CEF has
    // consumed the file name.
    std::u16string file_path_storage_;

    bool open_file_stream(const std::wstring &wpath, const std::u16string &mime)
    {
        if (wpath.empty())
            return false;

        std::filesystem::path fp(wpath);
        if (!std::filesystem::is_regular_file(fp))
            return false;

        file_path_storage_ = wide_to_u16(wpath);
        stream_ = cef_stream_reader_create_for_file(
            &CefStr::wrap(file_path_storage_));
        if (stream_ == nullptr)
            return false;

        stream_->seek(stream_, 0, SEEK_END);
        length_ = stream_->tell(stream_);
        stream_->seek(stream_, 0, SEEK_SET);
        mime_ = mime;
        return true;
    }

    bool open_string_stream(const std::string &body, const std::u16string &mime)
    {
        body_storage_ = body;
        stream_ = cef_stream_reader_create_for_data(
            (void *)body_storage_.data(), body_storage_.size());
        if (stream_ == nullptr)
            return false;

        length_ = static_cast<int64>(body_storage_.size());
        mime_ = mime;
        return true;
    }

    int _open(cef_request_t *request, int *handle_request, cef_callback_t *callback)
    {
        CefScopedStr url = request->get_url(request);
        std::u16string full((char16_t *)url.str, url.length);

        // Strip "https://watermelon" prefix (18 chars) to get the path.
        std::u16string path = (full.length() > 13) ? full.substr(13) : std::u16string();

        // Trim query string.
        size_t q = path.find(u'?');
        if (q != std::u16string::npos)
            path = path.substr(0, q);

        if (path.rfind(u"/skins/", 0) == 0)
        {
            if (path == u"/skins/version")
            {
                if (open_string_stream(get_skins_index_version(), u"text/plain"))
                {
                    *handle_request = 1;
                    return 1;
                }
            }

            if (open_file_stream(get_skins_index_path(), u"application/json"))
            {
                *handle_request = 1;
                return 1;
            }

            // Index file missing or unreadable -> serve an empty object
            // so preload.js gets a well-formed JSON response.
            static const char EMPTY_INDEX[] = "{}";
            stream_ = cef_stream_reader_create_for_data(
                (void *)EMPTY_INDEX, sizeof(EMPTY_INDEX) - 1);
            length_ = sizeof(EMPTY_INDEX) - 1;
            mime_ = u"application/json";
        }
        else if (path.rfind(u"/assets/", 0) == 0)
        {
            // Up to two candidate dirs per route: custom overrides first,
            // auto-generated assets as fallback. Splash stays auto-only.
            std::wstring try_dirs[2];
            size_t num_dirs = 0;
            std::u16string rel;

            if (path.rfind(u"/assets/background/", 0) == 0) {
                try_dirs[num_dirs++] = get_custom_background_previews_dir();
                try_dirs[num_dirs++] = get_background_previews_dir();
                rel = path.substr(19);
            } else if (path.rfind(u"/assets/splash/", 0) == 0) {
                try_dirs[num_dirs++] = get_previews_dir();
                rel = path.substr(15);
            } else if (path.rfind(u"/assets/tile/", 0) == 0) {
                try_dirs[num_dirs++] = get_custom_tile_previews_dir();
                try_dirs[num_dirs++] = get_tile_previews_dir();
                rel = path.substr(13);
            }

            if (num_dirs > 0 &&
                !rel.empty() &&
                rel.find(u'/') == std::u16string::npos &&
                rel.find(u'\\') == std::u16string::npos &&
                rel.find(u"..") == std::u16string::npos)
            {
                std::wstring decoded_filename;
                if (url_decode_path_component(rel, &decoded_filename))
                {
                    for (size_t i = 0; i < num_dirs; ++i)
                    {
                        if (try_dirs[i].empty())
                            continue;
                        std::wstring full_path = try_dirs[i] + L"\\" + decoded_filename;
                        if (open_file_stream(full_path, u"image/png"))
                        {
                            *handle_request = 1;
                            return 1;
                        }
                    }
                }
            }

            stream_ = nullptr;
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

struct WatermelonSchemeHandlerFactory : CefRefCount<cef_scheme_handler_factory_t>
{
    WatermelonSchemeHandlerFactory() : CefRefCount(this)
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
        return new WatermelonResourceHandler();
    }
};

void browser::register_watermelon_domain(cef_request_context_t *ctx)
{
    auto scheme = u"https"_s;
    auto domain = u"watermelon"_s;
    auto factory = new WatermelonSchemeHandlerFactory();

    ctx->register_scheme_handler_factory(ctx, &scheme, &domain, factory);
}
