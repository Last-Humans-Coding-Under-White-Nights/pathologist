// Uses `Late<IFoo>` with the template only forward-declared here; another
// unit defines it without inheriting its argument.
struct IFoo {
    virtual int Do() = 0;
};
template <class T> class Late;
class LateProxy : public Late<IFoo> {
public:
    int Do() override { return 5; }
};
