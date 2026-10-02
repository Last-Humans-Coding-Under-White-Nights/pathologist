// Declaration-only dependency: member-function-pointer aliases in a
// class template (#170).
template<class R, class C, class... Args>
struct Holder {
    using Member = R (C::*)(Args...) const;
    typedef R (C::*Named)(Args...) const;
    using Plain = R (C::*)(Args...);
    void useful();
};

int after_member_pointer();
