struct Outer {
    struct Inner { Inner(int value) {} };
    struct Declared { Declared(int value); };
    struct Deeper {
        struct Inner { Inner(int value) {} };
    };
    template<class T> struct TemplateInner { TemplateInner(int value) {} };
    struct Defaulted { Defaulted() = default; };
    struct Plain { int value; };
    struct UserBase { UserBase() {} };
    struct ImplicitDerived : UserBase { int value; };
};
