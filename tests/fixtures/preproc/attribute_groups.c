__attribute__((used)) const int before_type = 1;
const int after_declarator __attribute__((section(".trace.keep"))) = 2;
__declspec(align(16)) int msvc_aligned = 3;

#define PRINTF_LIKE __attribute__((format(printf, 1, 2)))
PRINTF_LIKE int trace_log(const char *format, ...);

__attribute__((noreturn)) void stop_now(void)
{
    for (;;) {}
}

__attribute__((constructor)) void ctor_hook(void) {}
__attribute__((destructor)) void dtor_hook(void) {}
__attribute__((__weak__)) int weak_value;
__attribute__((__visibility__("default"))) int exported_value;
__attribute__((__alias__("weak_value"))) extern int alias_value;
void release_value(int *value) { (void)value; }
void cleanup_scope(void) {
    __attribute__((__cleanup__(release_value))) int local_value = 0;
}
#define BASE_ATTRIBUTE __attribute__
#define ATTR_ALIAS BASE_ATTRIBUTE
#define ATTR_VIS __visibility__("default")
ATTR_ALIAS((ATTR_VIS)) int macro_export;
int macro_noise ATTR_ALIAS((used));
__declspec(selectany) int selected_value;
__declspec(dllexport) int dll_exported;
__declspec(dllimport) int dll_imported;
