// c_utils is not in the tree: DelayedRefSingleton is not declared (#184).
class RefSvc : public DelayedRefSingleton<RefSvc> {
public:
    void Run() {}
    void Stop() {}
};

// Its accessor returns `T &`, like Singleton's.
void ref_inherited() { RefSvc::GetInstance().Run(); }
void ref_spelled() { DelayedRefSingleton<RefSvc>::GetInstance().Run(); }
void ref_qualified() { OHOS::DelayedRefSingleton<RefSvc>::GetInstance().Run(); }
void ref_auto()
{
    auto &svc = RefSvc::GetInstance();
    svc.Stop();
}

// The issue's reproducer.
namespace OHOS {
class Machine : public DelayedRefSingleton<Machine> {
public:
    int Open();
};
int Machine::Open() { return 0; }
int Use() { return Machine::GetInstance().Open(); }
int UseSpelled() { return DelayedRefSingleton<Machine>::GetInstance().Open(); }
}

// Nothing else is guessed.
void ref_other_member() { DelayedRefSingleton<RefSvc>::Create().Run(); }
void ref_multi() { DelayedRefSingleton<RefSvc, RefSvc>::GetInstance().Run(); }
void ref_unresolved() { DelayedRefSingleton<Unknown>::GetInstance().Run(); }
// A template parameter spelled like a class names no class.
template <class RefSvc> void ref_dependent() { DelayedRefSingleton<RefSvc>::GetInstance().Run(); }

// A nested template named like c_utils' is not c_utils'.
class RefNest {
public:
    void Run() {}
};
template <class X> struct RefOuter {
    template <class T> struct DelayedRefSingleton {
        static X &GetInstance();
    };
};
void ref_nested() { RefOuter<RefNest>::DelayedRefSingleton<RefSvc>::GetInstance().Run(); }
