#include "anon.h"

void IAnon::Handle() {}

// An anonymous-namespace class deriving through a template-parameter base.
namespace {
class Svc : public AnonStub<IAnon> {
public:
    void Handle() override;
};
void Svc::Handle() {}
}

void Keep() { IAnon *a = new Svc(); a->Handle(); }
