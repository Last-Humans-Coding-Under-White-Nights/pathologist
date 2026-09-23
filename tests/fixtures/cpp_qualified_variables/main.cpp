struct X { int v; };
X global;
namespace ns { X *ptr = &global; }
struct Holder { static X *member; };
X *Holder::member = &global;
X *from_ns;
X *from_member;
X *seen_arg;
static void take(X **p) { seen_arg = *p; }
void use() {
    from_ns = ns::ptr;
    from_member = Holder::member;
    take(&ns::ptr);
}
