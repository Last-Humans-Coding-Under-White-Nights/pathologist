void fallback_only(void) {}
void (*global_callback)(void);
__attribute__((weak)) void hook(void) {
    global_callback = fallback_only;
    fallback_only();
}
