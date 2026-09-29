// The interface header is generated from IAtm.idl (#123).
#include "iatm.h"

namespace OHOS {
namespace Security {
struct Client {
    sptr<IAtm> proxy_;
    int Verify(unsigned id) { int s; return proxy_->VerifyAccessToken(id, s); }
};
} // namespace Security
} // namespace OHOS
