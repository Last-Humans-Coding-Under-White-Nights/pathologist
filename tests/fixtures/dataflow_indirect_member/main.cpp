struct State { void (*cb)(); };
void target() {}
void run(State *s) {
    s->cb = target;
    s->cb();
    State *alias;
    alias = s;
    alias->cb();
}
