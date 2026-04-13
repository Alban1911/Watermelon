#ifndef _HOOK_H_
#define _HOOK_H_

#include "platform.h"
#include <stdint.h>
#include <string.h>
#include <mutex>
#include <windows.h>

namespace hook
{
    struct Shellcode
    {
        uint8_t opcodes[12];

        Shellcode(intptr_t addr)
        {
            memset(opcodes, 0, sizeof(opcodes));
            // movabs rax [addr]
            opcodes[0] = 0x48;
            opcodes[1] = 0xB8;
            memcpy(&opcodes[2], &addr, sizeof(intptr_t));
            // push rax
            opcodes[10] = 0x50;
            // ret
            opcodes[11] = 0xC3;
        }
    };

    struct Restorable
    {
        Restorable(void *func, const void *code, size_t size)
            : func_(func), size_(size), backup_(new uint8_t[size])
        {
            memcpy(backup_, func, size);
            memcpy_safe(func, code, size);
        }

        ~Restorable()
        {
            memcpy_safe(func_, backup_, size_);
            delete[] backup_;
        }

        Restorable swap()
        {
            return Restorable(func_, backup_, size_);
        }

    private:
        void *func_;
        uint8_t *backup_;
        size_t size_;

        static bool memcpy_safe(void *dst, const void *src, size_t size)
        {
            DWORD op;
            BOOL success = VirtualProtect(dst, size, PAGE_EXECUTE_READWRITE, &op);
            if (success == 0)
                return false;
            memcpy(dst, src, size);
            success = VirtualProtect(dst, size, op, &op);
            return success != 0;
        }
    };

    template <typename>
    class Hook;

    template <typename R, typename... Args>
    class Hook<R (*)(Args...)>
    {
    public:
        using Fn = R (*)(Args...);

        ~Hook()
        {
            if (rest_)
            {
                std::lock_guard<std::mutex> _l(mutex_);
                {
                    delete rest_;
                }
            }
        }

        bool hook(Fn orig, Fn hook)
        {
            if (!orig || !hook)
                return false;

            orig_ = orig;
            Shellcode code(reinterpret_cast<intptr_t>(hook));
            rest_ = new Restorable((void *)orig, code.opcodes, sizeof(code.opcodes));
            return true;
        }

        bool hook(const char *lib, const char *proc, Fn hook)
        {
            if (HMODULE mod = GetModuleHandleA(lib))
                if (Fn orig = reinterpret_cast<Fn>(GetProcAddress(mod, proc)))
                    return this->hook(orig, hook);
            return false;
        }

        R operator()(Args... args)
        {
            std::lock_guard<std::mutex> _l(mutex_);
            {
                Restorable _t = rest_->swap();
                {
                    return orig_(args...);
                }
            }
        }

    protected:
        Fn orig_ = nullptr;
        Restorable *rest_ = nullptr;
        std::mutex mutex_;
    };
}

#endif
