using F = void (*)();
struct Callable {
    void operator()() {}
};
Callable maker(int v);
F factory();
void before();
void after();

void declared()
{
    before();
    F value = factory(), initialized_post(void (*cb)());
    value();
}
