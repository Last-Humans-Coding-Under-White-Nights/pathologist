#include "sptr.h"
#include "singleton.h"

template <typename T> class UniqueHolder {
public:
    static std::unique_ptr<T> Take();
};

template <typename T> class SptrHolder {
public:
    static sptr<T> Get();
};

class WrapSvc {
public:
    void Run() {}
};

void wrap_shared() { DelayedSingleton<WrapSvc>::GetInstance()->Run(); }
void wrap_unique() { UniqueHolder<WrapSvc>::Take()->Run(); }
void wrap_sptr() { SptrHolder<WrapSvc>::Get()->Run(); }
void wrap_shared_auto()
{
    auto p = DelayedSingleton<WrapSvc>::GetInstance();
    p->Run();
}
void wrap_sptr_auto()
{
    auto p = SptrHolder<WrapSvc>::Get();
    p->Run();
}
