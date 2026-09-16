void hook(void);
void caller(void) { hook(); }
void (*get_hook(void))(void) { return hook; }
