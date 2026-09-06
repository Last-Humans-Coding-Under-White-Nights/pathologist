__attribute__((used)) const int before_type = 1;
const int after_declarator __attribute__((section(".trace.keep"))) = 2;
__declspec(align(16)) int msvc_aligned = 3;

#define PRINTF_LIKE __attribute__((format(printf, 1, 2)))
PRINTF_LIKE int trace_log(const char *format, ...);

void stop_now(void) __attribute__((noreturn))
{
    for (;;) {}
}
