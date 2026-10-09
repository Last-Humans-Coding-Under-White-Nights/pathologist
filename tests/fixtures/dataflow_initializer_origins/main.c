typedef void (*Fn)(void);
struct Inner { Fn callback; char *pointer; };
struct Outer { struct Inner inner; };
char value;
struct Inner positional = {
    Later,
    &value,
};
struct Inner designated = {
    .callback = Later,
    .pointer = &value,
};
struct Outer nested = {
    .inner.callback = Later,
    .inner.pointer = &value,
};
struct Outer grouped = {
    .inner = {
        .callback = Later,
        .pointer = &value,
    },
};
void Later(void) {}
