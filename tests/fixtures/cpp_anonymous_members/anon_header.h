// A header's anonymous-namespace class, its member defined in other.cpp.
namespace {
struct HeaderImpl : Iface {
    void Fire() override;
};
} // namespace

// An overload set a header starts and the `.cpp` extends.
static void HeaderLog(int x) { OnHeaderLogInt(); }
