// Exercise header-cache import of reference-member metadata in another TU.
#include "types.hpp"

// These initializer lists lower after importing the header's field metadata.
ReferenceMember::ReferenceMember(Explicit &other) : ref_(other) {}
AliasReferenceMember::AliasReferenceMember(ExplicitRef other) : ref_(other) {}

void BindReference(Explicit &other) {
    ReferenceMember ref(other);
    AliasReferenceMember alias(other);
}
