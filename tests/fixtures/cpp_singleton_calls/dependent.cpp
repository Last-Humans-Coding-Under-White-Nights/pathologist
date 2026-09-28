#include "singleton.h"

// Template parameters spelled like real classes (review findings 3 and 4):
// inside the templates below `DepSvc` and `DepInh` are parameters, and an
// uninstantiated call on them names no class.
class DepSvc {
public:
    void Run() {}
};

class DepInh : public Singleton<DepInh> {
public:
    void Open() {}
};

template <class DepSvc> void dep_box() { Box<DepSvc>::Get()->Run(); }
template <class DepInh> void dep_inherited() { DepInh::GetInstance().Open(); }
