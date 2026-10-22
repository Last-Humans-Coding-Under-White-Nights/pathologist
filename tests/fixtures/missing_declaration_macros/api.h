#pragma once
namespace ffi {
FFI_EXPORT int Declared(const char *name, int *count);
extern "C" {
FFI_EXPORT int CDeclared(double value);
}
class Factory {
private:
    int before_field;
    void Before();
    DECLARE_SINGLETON(Factory)
public:
    long after_field;
    int After(const char *name);
};
struct Result { int value; };
FFI_EXPORT Result Custom(Result value);
}
