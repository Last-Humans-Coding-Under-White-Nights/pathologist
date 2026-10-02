char *source;
char *get() { return source; }
char *global = get();
void f() {
    global = get();
}
