int seed();
void inside();
void before();
void after();

void outer()
{
    before();
    auto run = [v = seed()]() {
        inside();
        return;
    };
    after();
    run();
}
