// C++17 `namespace A::B {` opens one scope per segment; C++20
// `namespace A::inline B {` names the inner one `B`.
namespace util {
void tag();
void tag() {}
}

namespace a::b {
class Deep {
public:
    int Go() { return 1; }
};
int use_deep() {
    Deep d;
    return d.Go();
}
namespace c {
int tri() {
    a::b::Deep d;
    return d.Go();
}
}
}

namespace a::b::c {
int again() { return tri(); }
}

namespace a::b {
using namespace util;
int inside() {
    tag();
    return 0;
}
}

// The directive above was scoped to its block.
namespace a {
int after() {
    tag();
    return 0;
}
}

namespace x::inline y {
int in_y() { return 2; }
}

int global_after() { return a::b::use_deep(); }

// A tab between `inline` and the segment name (C++20 `A::inline B`).
namespace tabbed::inline	ty {
int tabbed_leaf() { return 1; }
}
int tabbed_use() { return tabbed::ty::tabbed_leaf(); }
