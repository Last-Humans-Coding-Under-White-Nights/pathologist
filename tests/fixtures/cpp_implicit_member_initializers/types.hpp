typedef void (*Callback)();

struct Holder { Callback f; };
struct Plain { int x; };
struct Base { int x; };

struct R {
    R(Holder &h, const Plain &p);
    Holder &ref_;
    Plain copy_;
};

struct Derived : Base { Derived() : Base() {} };
struct Member { Plain value_; Member() : value_{} {} };

struct UserProvidedBase { UserProvidedBase() {} };
struct ImplicitDerived : UserProvidedBase { int x; };
struct DerivedMember {
    ImplicitDerived value_;
    DerivedMember() : value_() {}
};
struct DerivedCopyMember {
    ImplicitDerived value_;
    DerivedCopyMember(const ImplicitDerived &other) : value_(other) {}
};
struct DerivedAggregateMember {
    ImplicitDerived value_;
    DerivedAggregateMember(const UserProvidedBase &base) : value_{base, 7} {}
};
struct DerivedBase : ImplicitDerived {
    DerivedBase() : ImplicitDerived() {}
};
struct ExplicitDerived : UserProvidedBase { ExplicitDerived(int value) {} };
struct ExplicitDerivedMember {
    ExplicitDerived value_;
    ExplicitDerivedMember() : value_(7) {}
};

struct Defaulted {
    Defaulted() = default;
    Defaulted(const Defaulted &) = default;
};
struct DefaultedMember {
    Defaulted value_;
    DefaultedMember(const Defaulted &other) : value_(other) {}
};
struct DefaultedBase : Defaulted { DefaultedBase() : Defaulted{} {} };

struct Explicit {
    Explicit(const Explicit &) {}
};
struct ReferenceMember {
    Explicit &ref_;
    ReferenceMember(Explicit &other);
};
using ExplicitRef = Explicit &;
struct AliasReferenceMember {
    ExplicitRef ref_;
    AliasReferenceMember(ExplicitRef other);
};

struct CallbackReference {
    Callback &ref_;
    CallbackReference(Callback &cb) : ref_(cb) {}
    void Run() { ref_(); }
};
