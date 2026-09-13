// `using Alias = T;` is a typedef spelled the C++11 way (#91).
class AliasTarget { public: int Run() { return 1; } };
using ValueAlias = AliasTarget;
using PointerAlias = AliasTarget *;

int UsingLocal() {
    ValueAlias v;
    return v.Run();
}
int UsingPointer(PointerAlias p) { return p->Run(); }
int UsingWrapper(absent_ptr<ValueAlias> p) { return p->Run(); }

namespace ua {
class Scoped { public: int Run() { return 2; } };
using ScopedUsing = Scoped;
namespace deeper {
int UsingEnclosing(ScopedUsing s) { return s.Run(); }
}
}
int UsingQualified(ua::ScopedUsing s) { return s.Run(); }

using Callback = void (*)();
struct CbHolder { Callback cb; };
void CbTarget() {}
void UsingFnPtr(CbHolder h) {
    h.cb = CbTarget;
    h.cb();
}

// An alias declared in a class is a member of it: found from the class's
// own members, in and out of line, and by its qualified name elsewhere.
class WithMemberAlias {
public:
    using Target = AliasTarget;
    typedef AliasTarget *TargetPtr;
    Target member;
    int ViaMember() { return member.Run(); }
    int ViaParam(TargetPtr p);
};
int WithMemberAlias::ViaParam(TargetPtr p) { return p->Run(); }
int MemberAliasOutside(WithMemberAlias::Target t) { return t.Run(); }
int MemberAliasNoLeak(Target t) { return t.Run(); }
