// Declaration-only dependency: a class template with a computed base (#167).
template<class T> T make();

struct Base { void base(); };

template<class T>
struct Derived : decltype(make<T>()), Base {
    void member();
};

int after_decltype();
