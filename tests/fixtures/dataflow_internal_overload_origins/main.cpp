static void cb(int value) {}
static void cb(double value) {}
void (*arr[])(double) = { cb, &cb, cb };
void (*fp)(double) = cb;
void run(double value) {
    fp = cb;
    fp = &cb;
    fp = cb;
    arr[0](value);
    fp(value);
}
