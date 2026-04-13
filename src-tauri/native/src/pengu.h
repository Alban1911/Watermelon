#pragma once
#include "platform.h"

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

#include <windows.h>

#include <type_traits>
#include <atomic>
#include <string>
#include <vector>
#include <filesystem>

using path = std::filesystem::path;

/// Module handle for this DLL, set in DllMain.
extern HMODULE g_hModule;

/// LCUX used UTF-16 CEF strings.
#define CEF_STRING_TYPE_UTF16 1
#include "include/internal/cef_string.h"
#include "include/capi/cef_base_capi.h"

template<typename T>
struct remove_arg1;

/// Remove the first arg (self) from function sig.
template<typename R, typename Arg1, typename... Args>
struct remove_arg1<R(*)(Arg1, Args...)>
{
    using type = R(*)(Args...);
    using self = Arg1;
};

template <typename T>
struct method_traits;

/// Used to extract the method pairs with class.
template <typename T, typename R, typename... Args>
struct method_traits<R(T::*)(Args...)>
{
    using type = R(*)(Args...);
    using klass = T;
};

template <int id, typename This, typename M, typename R, typename Self, typename... Args>
struct self_bind_traits_base
{
    static M m_;

    static inline R CALLBACK invoke(Self self, Args ...args) noexcept {
        return (reinterpret_cast<This *>(self)->*m_)(args...);
    }
};

template <int id, typename This, typename M, typename R, typename Self, typename... Args>
/* typename */ M self_bind_traits_base<id, This, M, R, Self, Args...>::m_ = nullptr;

template <int id, typename, typename, typename>
struct self_bind_traits;

template <int id, typename This, typename M, typename R, typename Self, typename... Args>
struct self_bind_traits<id, This, M, R(*)(Self, Args...)>
    : self_bind_traits_base<id, This, M, R, Self, Args...> {};

template <int id, typename M, typename To>
static inline void self_bind(M from, To &to) noexcept
{
    using traits = self_bind_traits<id, typename method_traits<M>::klass, M, To>;
    if (traits::m_ == nullptr) traits::m_ = from;
    to = traits::invoke;
}

/// Use __COUNTER__ to make unique static variables on the same funtion sig.
/// `static_assert` to check method type when updating headers.
#define cef_bind_method(klass, m)                                                   \
    do {                                                                            \
        static_assert(std::is_same<method_traits<decltype(&klass::_##m)>::type,     \
            remove_arg1<decltype(m)>::type>::value, "Invalid method.");             \
        self_bind<__COUNTER__>(&klass::_##m, m);                                    \
    } while (0)

///
/// Basic reference counting for CAPI CEF objects.
///
template <typename T>
struct CefRefCount : public T
{
    template <typename U>
    CefRefCount(const U *) noexcept : T{}, ref_(1) {
        T::base.size = sizeof(U);
        T::base.add_ref = _Base_AddRef;
        T::base.release = _Base_Release;
        T::base.has_one_ref = _Base_HasOneRef;
        T::base.has_at_least_one_ref = _Base_HasAtLeastOneRef;
        self_delete_ = [](void *self) noexcept { delete static_cast<U *>(self); };
    }

    CefRefCount(nullptr_t) noexcept : CefRefCount(static_cast<T *>(nullptr)) {}

private:
    void(*self_delete_)(void *);
    std::atomic<size_t> ref_;

    static void CALLBACK _Base_AddRef(cef_base_ref_counted_t *_) noexcept {
        ++reinterpret_cast<CefRefCount *>(_)->ref_;
    }

    static int CALLBACK _Base_Release(cef_base_ref_counted_t *_) noexcept {
        CefRefCount *self = reinterpret_cast<CefRefCount *>(_);
        if (--self->ref_ == 0) {
            self->self_delete_(_);
            return 1;
        }
        return 0;
    }

    static int CALLBACK _Base_HasOneRef(cef_base_ref_counted_t *_) noexcept {
        return reinterpret_cast<CefRefCount *>(_)->ref_ == 1;
    }

    static int CALLBACK _Base_HasAtLeastOneRef(cef_base_ref_counted_t *_) noexcept {
        return reinterpret_cast<CefRefCount *>(_)->ref_ > 0;
    }
};

/// cef string interface
struct CefStrBase : cef_string_t
{
    CefStrBase();

    bool empty() const;

    bool equal(const char *that) const;
    bool contain(const char *sub) const;
    bool startw(const char *sub) const;
    bool endw(const char *sub) const;

    void copy(std::u16string &to) const;
    std::string to_utf8() const;
    std::u16string to_utf16() const;
    std::filesystem::path to_path() const;
};

struct CefStr : CefStrBase
{
    CefStr();
    ~CefStr();

    CefStr(const char *s, size_t len);
    CefStr(const char16_t *s, size_t len);
    CefStr(const std::string &s);
    CefStr(const std::u16string &s);

    cef_string_t forward();
    static const CefStrBase &borrow(const cef_string_t *s);
    static CefStr from_path(const path &path);

    static cef_string_t wrap(const std::u16string &utf16) {
        return cef_string_t{
            (char16 *)utf16.data(),
            utf16.length(),
            nullptr
        };
    }
};

struct CefScopedStr : CefStrBase
{
    CefScopedStr(cef_string_userfree_t uf);
    ~CefScopedStr();

    const cef_string_t *ptr() {
        return str_;
    }

private:
    cef_string_userfree_t str_;
};

/**
 * CefString UTF-16 literal.
*/
static inline cef_string_t operator""_s(const char16_t *s, size_t l)
{
    return cef_string_t{ (char16 *)s, l, nullptr };
}

namespace config
{
    path loader_dir();
    path datastore_path();
    path cache_dir();
    path league_dir();
    path plugins_dir();
    std::string disabled_plugins();

    namespace options
    {
        bool optimized_client();
        bool super_potato();
        bool isecure_mode();
        int debug_port();
    }
}

namespace file
{
    bool is_dir(const path &path);
    bool is_file(const path &path);
    bool is_symlink(const path &path);
    bool read_file(const path &path, void **buffer, size_t *length);
    bool write_file(const path &path, const void *buffer, size_t length);
    std::vector<path> read_dir(const path &dir);
}

namespace dialog
{
    static inline void alert(const char *message, const char *caption) {
        MessageBoxA(NULL, message, caption,
            MB_ICONINFORMATION | MB_OK | MB_TOPMOST);
    }
}

namespace dylib
{
    void *find_lib(const char *name);
    void *find_proc(void *lib, const char *proc);
    void *find_memory(const void *rladdr, const char *pattern);
}
