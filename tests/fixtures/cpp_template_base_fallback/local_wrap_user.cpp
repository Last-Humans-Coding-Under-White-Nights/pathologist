// A file-local class whose declared template base another unit defines.
struct ILocal {
    virtual void Local() = 0;
};
template <class T> class LocalWrap;
namespace {
class LocalImpl : public LocalWrap<ILocal> {
public:
    void Local() override {}
};
}
void calllocal(ILocal *l) { l->Local(); }
