// c_utils is not in the tree: neither singleton template is declared.
class MissSvc {
public:
    void Run() {}
};

class MissDb {
public:
    void Open() {}
};

// The two well-known spellings are recovered.
void miss_delayed() { DelayedSingleton<MissSvc>::GetInstance()->Run(); }
void miss_singleton() { Singleton<MissDb>::GetInstance().Open(); }
void miss_qualified() { OHOS::DelayedSingleton<MissSvc>::GetInstance()->Run(); }
void miss_auto()
{
    auto p = DelayedSingleton<MissSvc>::GetInstance();
    p->Run();
}

// Nothing else is guessed.
void miss_wrapper() { Wrapper<MissSvc>::Get()->Run(); }
void miss_other_member() { DelayedSingleton<MissSvc>::Create()->Run(); }
void miss_unresolved() { DelayedSingleton<Unknown>::GetInstance()->Run(); }
void miss_multi() { DelayedSingleton<MissSvc, MissDb>::GetInstance()->Run(); }

// An accessor inherited from an undeclared base is typed as the base's.
class MissInh : public DelayedSingleton<MissInh> {
public:
    void Run() {}
};
void miss_inherited() { MissInh::GetInstance()->Run(); }

// A template parameter spelled like a class guesses nothing (review finding 5).
template <class MissSvc> void miss_dependent() { DelayedSingleton<MissSvc>::GetInstance()->Run(); }

// A nested template named like c_utils' is not c_utils' (R2-5).
class NestA {
public:
    void Run() {}
};
template <class X> struct Outer {
    template <class T> struct DelayedSingleton {
        static T *GetInstance();
    };
};
void nested_singleton() { Outer<NestA>::DelayedSingleton<MissSvc>::GetInstance()->Run(); }
