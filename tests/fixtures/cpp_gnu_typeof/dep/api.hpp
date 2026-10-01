// Declaration-only dependency: GNU typeof in a class template (#169).
template<class T, class U>
struct sum_result {
    typedef __typeof__(T() + U()) type;
    typedef __typeof__(int) itype;
    void useful();
};

int after_typeof();
