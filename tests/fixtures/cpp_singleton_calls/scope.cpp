#include "singleton.h"

namespace app {
class ScopeSvc {
public:
    void Run() {}
};

void relative() { Box<ScopeSvc>::Get()->Run(); }
}

void rooted() { Box<::app::ScopeSvc>::Get()->Run(); }
void namespaced_template() { lib::NsBox<app::ScopeSvc>::Get()->Run(); }

namespace shade {
class ScopeSvc {
public:
    void Run() {}
};

void shadowed() { Box<ScopeSvc>::Get()->Run(); }
}

namespace lib {
void template_relative() { NsBox<shade::ScopeSvc>::Get()->Run(); }
}
