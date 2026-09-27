#include "singleton.h"

// CRTP singletons: the accessor is declared on the template base only.
class InhSvc : public DelayedSingleton<InhSvc> {
public:
    void Run() {}
};

class InhDb : public Singleton<InhDb> {
public:
    void Open() {}
};

// An intermediate base carries the template argument.
class InhMidSvc;
class ServiceBase : public DelayedSingleton<InhMidSvc> {};
class InhMidSvc : public ServiceBase {
public:
    void Run() {}
};

// A derived declaration hides the base's accessor.
class InhOther {
public:
    void Run() {}
};
class InhHide : public DelayedSingleton<InhHide> {
public:
    static InhOther *GetInstance();
    void Run() {}
};

void inh_shared() { InhSvc::GetInstance()->Run(); }
void inh_ref() { InhDb::GetInstance().Open(); }
void inh_mid() { InhMidSvc::GetInstance()->Run(); }
void inh_auto()
{
    auto p = InhSvc::GetInstance();
    p->Run();
}
void inh_hide() { InhHide::GetInstance()->Run(); }
