#pragma once

// Class-template accessors declared as c_utils declares them: the return type
// names the template parameter, and the definitions live elsewhere.
template <typename T> class Box {
public:
    static T *Get();
};

// c_utils' DelayedSingleton; `std` itself is never declared in the tree.
template <typename T> class DelayedSingleton {
public:
    static std::shared_ptr<T> GetInstance();
};

template <typename T> class Singleton {
public:
    static T &GetInstance();
};

template <typename T> class Maker {
public:
    static T Make();
};

// A defined accessor, so its edge resolves directly.
template <typename T> class DefBox {
public:
    static T *Get() { return nullptr; }
};

namespace lib {
template <typename T> class NsBox {
public:
    static T *Get();
};
}
