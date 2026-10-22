#include "types.h"
#include "foo_proxy.h"

namespace OHOS { namespace HDI { namespace Foo { namespace V1_0 {
struct Client {
    sptr<IFoo> remote;
    int Run(const std::string& message, std::string& reply, FooInfo& info)
    {
        remote->GetData(info);
        remote->InfoTest(info, info);
        remote->Reset();
        return remote->Ping(message, reply);
    }
};
} } } }
