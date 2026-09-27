#include "singleton.h"

class DeclSvc {
public:
    void Run() {}
};

class DeclDb {
public:
    void Open() {}
};

void decl_ptr() { Box<DeclSvc>::Get()->Run(); }
void decl_ref() { Singleton<DeclDb>::GetInstance().Open(); }
void decl_value() { Maker<DeclDb>::Make().Open(); }
void decl_auto()
{
    auto p = Box<DeclSvc>::Get();
    p->Run();
}
void decl_auto_ref()
{
    auto &db = Singleton<DeclDb>::GetInstance();
    db.Open();
}
void decl_defined() { DefBox<DeclSvc>::Get()->Run(); }
