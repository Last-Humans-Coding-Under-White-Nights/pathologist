#pragma once
// Representative available definitions: these must expand before recovery.
#define FFI_EXPORT __attribute__((visibility("default")))
#define DECLARE_SINGLETON(ClassName) \
    private: ClassName(); ~ClassName(); \
    public: static ClassName &GetInstance();
