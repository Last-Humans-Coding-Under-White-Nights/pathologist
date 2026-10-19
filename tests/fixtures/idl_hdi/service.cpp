#include "types.h"
#include "foo_stub.h"

namespace OHOS { namespace HDI { namespace Foo { namespace V1_0 {
class FooService : public IFoo {
public:
    int Ping(const std::string& message, std::string& reply) override;
    int GetData(FooInfo& info) override;
    int InfoTest(const FooInfo& input, FooInfo& output) override;
    int Reset() override;
};

int FooService::Ping(const std::string& message, std::string& reply) { reply = message; return 0; }
int FooService::GetData(FooInfo& info) { info.value = 1; return 0; }
int FooService::InfoTest(const FooInfo& input, FooInfo& output) { output = input; return 0; }
int FooService::Reset() { return 0; }
} } } }
