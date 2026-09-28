// The declared-template fixture from issue #121, verbatim apart from the
// `std::shared_ptr` it names, which the tree does not declare.
template <typename T> class DelayedSingleton { public: static std::shared_ptr<T> GetInstance(); };
template <typename T> class Singleton        { public: static T &GetInstance(); };
class Svc : public DelayedSingleton<Svc> { public: void Run() {} };
class Db  : public Singleton<Db>         { public: void Open() {} };
class Other { public: void Go() {} };
void f1() { DelayedSingleton<Svc>::GetInstance()->Run(); }
void f2() { Svc::GetInstance()->Run(); }
void f3() { Db::GetInstance().Open(); }
void f4() { Singleton<Db>::GetInstance().Open(); }
void f5() { DelayedSingleton<Other>::GetInstance()->Go(); }
void f6() { auto p = DelayedSingleton<Other>::GetInstance(); p->Go(); }
