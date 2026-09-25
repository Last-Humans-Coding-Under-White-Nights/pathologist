// An interface and a stub template shared by two translation units.
struct IAnon { virtual void Handle(); };
template <class I> class AnonStub : public I {};
