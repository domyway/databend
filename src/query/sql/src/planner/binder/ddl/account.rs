// Copyright 2021 Datafuse Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use chrono::Utc;
use databend_common_ast::ast::AccountMgrLevel;
use databend_common_ast::ast::AccountMgrSource;
use databend_common_ast::ast::AlterUserStmt;
use databend_common_ast::ast::CreateUserStmt;
use databend_common_ast::ast::GrantStmt;
use databend_common_ast::ast::RevokeStmt;
use databend_common_exception::Result;
use databend_common_meta_app::principal::AuthInfo;
use databend_common_meta_app::principal::GrantObject;
use databend_common_meta_app::principal::PrincipalIdentity;
use databend_common_meta_app::principal::UserOption;
use databend_common_meta_app::principal::UserPrivilegeSet;
use databend_common_users::UserApiProvider;

use crate::plans::AlterUserPlan;
use crate::plans::CreateUserPlan;
use crate::plans::GrantPrivilegePlan;
use crate::plans::GrantRolePlan;
use crate::plans::Plan;
use crate::plans::RevokePrivilegePlan;
use crate::plans::RevokeRolePlan;
use crate::Binder;

impl Binder {
    #[async_backtrace::framed]
    pub(in crate::planner::binder) async fn bind_grant(
        &mut self,
        stmt: &GrantStmt,
    ) -> Result<Plan> {
        let GrantStmt { source, principal } = stmt;

        match source {
            AccountMgrSource::Role { role } => {
                let plan = GrantRolePlan {
                    principal: principal.clone(),
                    role: role.clone(),
                };
                Ok(Plan::GrantRole(Box::new(plan)))
            }
            AccountMgrSource::ALL { level } => {
                // ALL PRIVILEGES have different available privileges set on different grant objects
                // Now in this case all is always true.
                let grant_object = self.convert_to_grant_object(level).await?;
                let priv_types = grant_object.available_privileges(false);
                let plan = GrantPrivilegePlan {
                    principal: principal.clone(),
                    on: grant_object,
                    priv_types,
                };
                Ok(Plan::GrantPriv(Box::new(plan)))
            }
            AccountMgrSource::Privs { privileges, level } => {
                let grant_object = self.convert_to_grant_object(level).await?;
                let mut priv_types = UserPrivilegeSet::empty();
                for x in privileges {
                    priv_types.set_privilege(*x);
                }
                let plan = GrantPrivilegePlan {
                    principal: principal.clone(),
                    on: grant_object,
                    priv_types,
                };
                Ok(Plan::GrantPriv(Box::new(plan)))
            }
        }
    }

    #[async_backtrace::framed]
    pub(in crate::planner::binder) async fn bind_revoke(
        &mut self,
        stmt: &RevokeStmt,
    ) -> Result<Plan> {
        let RevokeStmt { source, principal } = stmt;

        match source {
            AccountMgrSource::Role { role } => {
                let plan = RevokeRolePlan {
                    principal: principal.clone(),
                    role: role.clone(),
                };
                Ok(Plan::RevokeRole(Box::new(plan)))
            }
            AccountMgrSource::ALL { level } => {
                // ALL PRIVILEGES have different available privileges set on different grant objects
                // Now in this case all is always true.
                let grant_object = self.convert_to_revoke_grant_object(level).await?;
                // Note if old version `grant all on db.*/db.t to user`, the user will contains ownership privilege.
                // revoke all need to revoke it.
                let priv_types = match principal {
                    PrincipalIdentity::User(_) => grant_object[0].available_privileges(true),
                    PrincipalIdentity::Role(_) => grant_object[0].available_privileges(false),
                };
                let plan = RevokePrivilegePlan {
                    principal: principal.clone(),
                    on: grant_object,
                    priv_types,
                };
                Ok(Plan::RevokePriv(Box::new(plan)))
            }
            AccountMgrSource::Privs { privileges, level } => {
                let grant_object = self.convert_to_revoke_grant_object(level).await?;
                let mut priv_types = UserPrivilegeSet::empty();
                for x in privileges {
                    priv_types.set_privilege(*x);
                }
                let plan = RevokePrivilegePlan {
                    principal: principal.clone(),
                    on: grant_object,
                    priv_types,
                };
                Ok(Plan::RevokePriv(Box::new(plan)))
            }
        }
    }

    pub(in crate::planner::binder) async fn convert_to_grant_object(
        &self,
        source: &AccountMgrLevel,
    ) -> Result<GrantObject> {
        // TODO fetch real catalog
        let catalog_name = self.ctx.get_current_catalog();
        let tenant = self.ctx.get_tenant();
        let catalog = self.ctx.get_catalog(&catalog_name).await?;
        match source {
            AccountMgrLevel::Global => Ok(GrantObject::Global),
            AccountMgrLevel::Table(database_name, table_name) => {
                let database_name = database_name
                    .clone()
                    .unwrap_or_else(|| self.ctx.get_current_database());
                let db_id = catalog
                    .get_database(&tenant, &database_name)
                    .await?
                    .get_db_info()
                    .ident
                    .db_id;
                let table_id = catalog
                    .get_table(&tenant, &database_name, table_name)
                    .await?
                    .get_id();
                Ok(GrantObject::TableById(catalog_name, db_id, table_id))
            }
            AccountMgrLevel::Database(database_name) => {
                let database_name = database_name
                    .clone()
                    .unwrap_or_else(|| self.ctx.get_current_database());
                let db_id = catalog
                    .get_database(&tenant, &database_name)
                    .await?
                    .get_db_info()
                    .ident
                    .db_id;
                Ok(GrantObject::DatabaseById(catalog_name, db_id))
            }
            AccountMgrLevel::UDF(udf) => Ok(GrantObject::UDF(udf.clone())),
            AccountMgrLevel::Stage(stage) => Ok(GrantObject::Stage(stage.clone())),
        }
    }

    // Some old query version use GrantObject::Table store table name.
    // So revoke need compat the old version.
    pub(in crate::planner::binder) async fn convert_to_revoke_grant_object(
        &self,
        source: &AccountMgrLevel,
    ) -> Result<Vec<GrantObject>> {
        // TODO fetch real catalog
        let catalog_name = self.ctx.get_current_catalog();
        let tenant = self.ctx.get_tenant();
        let catalog = self.ctx.get_catalog(&catalog_name).await?;
        match source {
            AccountMgrLevel::Global => Ok(vec![GrantObject::Global]),
            AccountMgrLevel::Table(database_name, table_name) => {
                let database_name = database_name
                    .clone()
                    .unwrap_or_else(|| self.ctx.get_current_database());
                let db_id = catalog
                    .get_database(&tenant, &database_name)
                    .await?
                    .get_db_info()
                    .ident
                    .db_id;
                let table_id = catalog
                    .get_table(&tenant, &database_name, table_name)
                    .await?
                    .get_id();
                Ok(vec![
                    GrantObject::TableById(catalog_name.clone(), db_id, table_id),
                    GrantObject::Table(catalog_name.clone(), database_name, table_name.clone()),
                ])
            }
            AccountMgrLevel::Database(database_name) => {
                let database_name = database_name
                    .clone()
                    .unwrap_or_else(|| self.ctx.get_current_database());
                let db_id = catalog
                    .get_database(&tenant, &database_name)
                    .await?
                    .get_db_info()
                    .ident
                    .db_id;
                Ok(vec![
                    GrantObject::DatabaseById(catalog_name.clone(), db_id),
                    GrantObject::Database(catalog_name.clone(), database_name),
                ])
            }
            AccountMgrLevel::UDF(udf) => Ok(vec![GrantObject::UDF(udf.clone())]),
            AccountMgrLevel::Stage(stage) => Ok(vec![GrantObject::Stage(stage.clone())]),
        }
    }

    #[async_backtrace::framed]
    pub(in crate::planner::binder) async fn bind_create_user(
        &mut self,
        stmt: &CreateUserStmt,
    ) -> Result<Plan> {
        let CreateUserStmt {
            create_option,
            user,
            auth_option,
            user_options,
        } = stmt;
        let mut user_option = UserOption::default();
        for option in user_options {
            option.apply(&mut user_option);
        }
        UserApiProvider::instance()
            .verify_password(
                &self.ctx.get_tenant(),
                &user_option,
                auth_option,
                None,
                None,
            )
            .await?;

        let plan = CreateUserPlan {
            create_option: create_option.clone(),
            user: user.clone(),
            auth_info: AuthInfo::create2(&auth_option.auth_type, &auth_option.password)?,
            user_option,
            password_update_on: Some(Utc::now()),
        };
        Ok(Plan::CreateUser(Box::new(plan)))
    }

    #[async_backtrace::framed]
    pub(in crate::planner::binder) async fn bind_alter_user(
        &mut self,
        stmt: &AlterUserStmt,
    ) -> Result<Plan> {
        let AlterUserStmt {
            user,
            auth_option,
            user_options,
        } = stmt;
        // None means current user
        let user_info = if user.is_none() {
            self.ctx.get_current_user()?
        } else {
            UserApiProvider::instance()
                .get_user(&self.ctx.get_tenant(), user.clone().unwrap())
                .await?
        };

        let mut user_option = user_info.option.clone();
        for option in user_options {
            option.apply(&mut user_option);
        }

        // None means no change to make
        let new_auth_info = if let Some(auth_option) = &auth_option {
            let auth_info = user_info
                .auth_info
                .alter2(&auth_option.auth_type, &auth_option.password)?;
            // verify the password if changed
            UserApiProvider::instance()
                .verify_password(
                    &self.ctx.get_tenant(),
                    &user_option,
                    auth_option,
                    Some(&user_info),
                    Some(&auth_info),
                )
                .await?;
            if user_info.auth_info == auth_info {
                None
            } else {
                Some(auth_info)
            }
        } else {
            None
        };

        let new_user_option = if user_option == user_info.option {
            None
        } else {
            Some(user_option)
        };
        let plan = AlterUserPlan {
            user: user_info.identity(),
            auth_info: new_auth_info,
            user_option: new_user_option,
        };

        Ok(Plan::AlterUser(Box::new(plan)))
    }
}
