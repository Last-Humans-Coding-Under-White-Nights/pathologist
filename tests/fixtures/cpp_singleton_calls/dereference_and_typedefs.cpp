// Dereferenced smart-pointer references and fixed-width typedef parameters.
// R16-3: `*sp` of a reference to a smart pointer calls its `operator*`.
class R16Foo {};
template <class T> class R16Sptr {
public:
    T &operator*() const;
};
void R16Use(const R16Sptr<R16Foo> &p);
void R16Use(R16Foo &f);
void r16_deref(const R16Sptr<R16Foo> &sp) { R16Use(*sp); }

// R16-4: a fixed-width typedef out of view is an integer of its own, so the
// definition meets its prototype.
struct R16Writer {
    void Write(uint32_t v);
    void Write(uint64_t v);
};
void R16Writer::Write(uint32_t v) {}
void R16Writer::Write(uint64_t v) {}
