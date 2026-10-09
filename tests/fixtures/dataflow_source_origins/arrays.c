void first(void) {}
void second(void) {}
void (*handlers[])(void) = { first, second };
void remote_first(void);
void remote_second(void);
void (*remote_handlers[])(void) = {
    remote_first,
    &remote_second,
    remote_first
};
