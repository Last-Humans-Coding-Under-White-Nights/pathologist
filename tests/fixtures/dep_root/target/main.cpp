#include <refbase.h>

class TargetService : public DepBase {
public:
    void ServiceAction() {
    }
    void BaseMethod() override {
        ServiceAction();
    }
};

// A declaration from the dependency root is a resolvable callee; the call
// edge lands on a function marked is_dep.
void UseDependency() {
    dummy_dep_callee();
}

int main() {
    sptr<TargetService> session = new TargetService();
    session->BaseMethod();
    session->ServiceAction();
    UseDependency();
    return 0;
}
