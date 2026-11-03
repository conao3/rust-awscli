import json, sys
from awscli.customizations.emr.emr import register_commands
from awscli.customizations.emrcontainers import update_role_trust_policy as u, create_role_associations as c, delete_role_associations as d
class S: pass
ct={}
register_commands(ct, None)
out={}
def clean(t):
    r=[]
    for a in t:
        a=dict(a)
        a.pop('help_text',None); a.pop('synopsis',None); a.pop('group_name',None)
        r.append(a)
    return r
for k,v in ct.items():
    out[k]=clean(v.ARG_TABLE)
for cls in [u.UpdateRoleTrustPolicyCommand, c.CreateRoleAssociationsCommand, d.DeleteRoleAssociationsCommand]:
    out['emr-containers '+cls.NAME]=clean(cls.ARG_TABLE)
print(json.dumps(out, indent=1))
