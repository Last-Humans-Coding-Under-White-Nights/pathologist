template <class F> void call(F f) { f(); }
void f() {
    call([]() __attribute__((unused)) { return 0; });
}
