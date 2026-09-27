struct PA;
template <class T> struct PDefined;
struct PB : PDefined<PA> {
    virtual void P();
};
