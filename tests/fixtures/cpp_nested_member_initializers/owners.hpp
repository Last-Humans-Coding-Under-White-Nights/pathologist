#include "nested.hpp"

struct Owner {
    Outer::Inner value;
    Owner() : value(7) {}
};
struct DeclaredOwner {
    Outer::Declared value;
    DeclaredOwner() : value(7) {}
};
struct DeeperOwner {
    Outer::Deeper::Inner value;
    DeeperOwner() : value(7) {}
};
struct TemplateOwner {
    Outer::TemplateInner<int> value;
    TemplateOwner() : value(7) {}
};
struct DefaultedOwner {
    Outer::Defaulted value;
    DefaultedOwner() : value() {}
};
struct PlainOwner {
    Outer::Plain value;
    PlainOwner() : value() {}
};
struct ImplicitDerivedOwner {
    Outer::ImplicitDerived value;
    ImplicitDerivedOwner() : value() {}
};
struct ImplicitDerivedCopyOwner {
    Outer::ImplicitDerived value;
    ImplicitDerivedCopyOwner(const Outer::ImplicitDerived &other) : value(other) {}
};
struct ImplicitDerivedAggregateOwner {
    Outer::ImplicitDerived value;
    ImplicitDerivedAggregateOwner(const Outer::UserBase &base) : value{base, 7} {}
};
struct ImplicitDerivedBaseOwner : Outer::ImplicitDerived {
    ImplicitDerivedBaseOwner() : ImplicitDerived() {}
};
