// Shared by the two units that each define a file-local `Impl`.
struct ILocal { virtual void L(); };
struct IExtra { virtual void E(); };
template <class A, class B> struct Pair : A, B {};
