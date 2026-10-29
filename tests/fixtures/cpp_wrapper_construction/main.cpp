namespace std {
template<class T, class Deleter> class unique_ptr {
public:
    unique_ptr() {}
    int WrapperMember() { return 1; }
};
}
class Item {};
class Deleter {};

int declared_wrapper() {
    auto p = std::unique_ptr<Item, Deleter>();
    return p.WrapperMember();
}
int declared_wrapper_chain() {
    return std::unique_ptr<Item, Deleter>().WrapperMember();
}

namespace lib {
class Receiver {};
class Result {
public:
    int Run() { return 2; }
};
template<class T> Result sptr(T *pointer);
}

int adl_factory(lib::Receiver *raw) {
    auto result = sptr<lib::Receiver>(raw);
    return result.Run();
}
int adl_factory_chain(lib::Receiver *raw) {
    return sptr<lib::Receiver>(raw).Run();
}
