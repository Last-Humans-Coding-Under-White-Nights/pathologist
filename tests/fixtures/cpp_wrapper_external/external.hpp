namespace lib {
template<class T> struct Outer {
    template<class U> struct sptr {
        U *ptr;
        sptr(U *p) : ptr(p) {}
        U *operator->() { return ptr; }
    };
};
}
