namespace app {
struct Chain {
    Chain next();
    void finish();
};
Chain make();
void chained() {
    make().next()
        .next().finish();
}
auto unknown();
void untyped() {
    auto value = unknown();
    value.run();
    unknown().run();
}
}
