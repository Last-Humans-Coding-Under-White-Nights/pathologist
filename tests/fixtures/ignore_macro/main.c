extern void before_call(void);
extern void log_call1(void);
extern void log_call2(void);
extern void log_call3(void);
extern void after_call(void);

#define LOG(msg) do { \
    log_call1(); \
    log_call2(); \
    log_call3(); \
} while (0)

void test_fn(void) {
    before_call();
    LOG("hello world");
    after_call();
}
