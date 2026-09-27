// A guess from an earlier unit does not block a declared base (prec_b).
struct PB {
    virtual void P();
};
template <class T> class PUndef;
struct PA : PUndef<PB> {
    void P();
};
