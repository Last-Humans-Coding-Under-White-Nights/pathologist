class Item { public: int Run() { return 2; } };
namespace unrelated {
template<class T> class sptr {
public:
    sptr(T *) {}
    int Run() { return 9; }
};
}
namespace lib {
template<class T> class sptr {
    T *refs_;
public:
    sptr(T *p) : refs_(p) {}
    T *operator->() const { return refs_; }
    T *GetRefPtr() const { return refs_; }
    int Run() { return 1; }
};
int call(Item *item) {
    auto p = sptr<Item>(item);
    return p.Run();
}
int chain(Item *item) { return sptr<Item>(item).Run(); }
int auto_arrow(Item *item) { auto p = sptr<Item>(item); return p->Run(); }
int arrow_chain(Item *item) { return sptr<Item>(item)->Run(); }
int get_ref(Item *item) { auto p = sptr<Item>(item); return p.GetRefPtr()->Run(); }
int declared_arrow(Item *item) { sptr<Item> p(item); return p->Run(); }
}
