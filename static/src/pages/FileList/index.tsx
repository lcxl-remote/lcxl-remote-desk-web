import { listFiles } from '@/services/desk/listFiles';
import { deleteFile } from '@/services/desk/deleteFile';
import { formatSize } from '@/utils/format_utils';
import { SearchOutlined } from '@ant-design/icons';
import type { ActionType, ColumnsState, ProColumns, ProDescriptionsItemProps, ProFormInstance } from '@ant-design/pro-components';
import {
  FooterToolbar,
  PageContainer,
  ProDescriptions,
  ProTable,
} from '@ant-design/pro-components';
import { FormattedMessage, useIntl, history } from '@umijs/max';
import { Button, Drawer, Input, message, Popconfirm, Select, SelectProps } from 'antd';
import React, { useRef, useState } from 'react';


/**
 *  Delete node
 * @zh-CN 删除节点
 *
 * @param selectedRows
 */
const handleRemove = async (selectedRows: API.FileInfo[], intl: any) => {
  const hide = message.loading(intl.formatMessage({ id: 'pages.fileList.deleting' }));
  if (!selectedRows) return true;
  try {
    for (const index in selectedRows) {
      const request: API.DeleteFileRequest = {
        delete_permanently: false,
        file_path: selectedRows[index].path,
      }
      await deleteFile(request);
    }
    hide();
    message.success(intl.formatMessage({ id: 'pages.fileList.deleteSuccess' }));
    return true;
  } catch (error) {
    hide();
    message.error(intl.formatMessage({ id: 'pages.fileList.deleteFailed' }));
    return false;
  }
};

const TableList: React.FC = () => {
  /**
   * @en-US The pop-up window of the distribution update window
   * @zh-CN 分布更新窗口的弹窗
  * */
  const formRef = useRef<ProFormInstance>();
  const [showDetail, setShowDetail] = useState<boolean>(false);

  const actionRef = useRef<ActionType>();
  const [currentRow, setCurrentRow] = useState<API.FileInfo>();
  const [selectedRowsState, setSelectedRows] = useState<API.FileInfo[]>([]);
  const [columnsStateMap, setColumnsStateMap] = useState<
    Record<string, ColumnsState>
  >({
    "file_info,scan_time": {
      show: false,
      order: 0,
    },
    "file_info,inode_info,md5": {
      show: false,
      order: 0,
    },
    "file_info,inode_info,created": {
      show: false,
      order: 0,
    },
    "file_info,inode_info,modified": {
      show: false,
      order: 0,
    },
  });
  /**
   * @en-US International configuration
   * @zh-CN 国际化配置
   * */
  const intl = useIntl();

  const columns: ProColumns<API.FileInfo>[] = [
    {
      title: (
        <FormattedMessage
          id="pages.searchTable.filePath"
        />
      ),
      hideInTable: true,
      dataIndex: ["path"],
      valueType: 'text',
    },
    {
      title: (
        <FormattedMessage
          id="pages.searchTable.updateForm.ruleName.nameLabel"
        />
      ),
      dataIndex: ["name"],
      hideInSearch: true,
      hideInDescriptions: true,
      render: (dom, entity) => {
        let content = entity.name;
        if (!content) {
          content = entity.path;
        }
        if (!entity.is_dir) {
          return content;
        }
        return (
          <a
            onClick={() => {
              if (!formRef.current) {
                return;
              }
              formRef.current.setFieldsValue({
                path: entity.path,
              });
              formRef.current.submit();
            }}
          >
            {content}
          </a>
        );
      },
    },
    {
      title: <FormattedMessage id="pages.searchTable.fileSize" />,
      dataIndex: ['size'],
      hideInSearch: true,
      sorter: true,
      renderText: (val: number) => {
        return formatSize(val);
      }
      ,
    },
    {
      title: (
        <FormattedMessage
          id="pages.searchTable.fileCreatedTime"
        />
      ),
      //sorter: true,
      hideInSearch: true,
      dataIndex: ["created"],
      valueType: 'dateTime',
      renderFormItem: (item, { defaultRender, ...rest }, form) => {
        return defaultRender(item);
      },
    },
    {
      title: (
        <FormattedMessage
          id="pages.searchTable.fileModifiedTime"
        />
      ),
      //sorter: true,
      hideInSearch: true,
      dataIndex: ["modified"],
      valueType: 'dateTime',
      renderFormItem: (item, { defaultRender, ...rest }, form) => {
        return defaultRender(item);
      },
    },
    {
      title: (
        <FormattedMessage
          id="pages.searchTable.fileAccessedTime"
        />
      ),
      //sorter: true,
      hideInSearch: true,
      dataIndex: ["accessed"],
      valueType: 'dateTime',
      renderFormItem: (item, { defaultRender, ...rest }, form) => {
        return defaultRender(item);
      },
    },
    {
      title: <FormattedMessage id="pages.searchTable.titleOption" />,
      dataIndex: 'option',
      valueType: 'option',
      render: (_, record) => [
        <a
            onClick={() => {
              setCurrentRow(record);
              setShowDetail(true);
            }}
          >
           <FormattedMessage
            id="pages.searchTable.detail"
          />
          </a>,
        <Popconfirm
          title={intl.formatMessage({ id: "pages.searchTable.optionDeleteConfirmTitle" })}
          description={intl.formatMessage({ id: "pages.searchTable.optionDeleteConfirmDescription" })}
          onConfirm={
            async () => {
              console.log(intl.formatMessage({ id: 'pages.fileList.startDeleteFile' }, { path: record.path }));
              try {
                const response = await deleteFile({
                  file_path: record.path,
                  delete_permanently: false,
                });
                console.log(intl.formatMessage({ id: 'pages.fileList.deletedFile' }, { path: record.path, response }));
                setCurrentRow(undefined);
                actionRef.current?.reloadAndRest?.();
              } catch (err) {
                console.log(intl.formatMessage({ id: 'pages.fileList.requestDeleteFileError' }, { error: err }))
              }
            }
          }
        >
          <a key="config">
            <FormattedMessage id="pages.searchTable.deletion" />
          </a>
        </Popconfirm>,
      ],
    },
  ];

  return (
    <PageContainer>
      <ProTable<API.FileInfo, API.FileInfo & {
        search_file_modified_time?: string[];
        search_file_created_time?: string[];
        file_extention_list?: string[];
        search_file_size?: number[];
      }>
        headerTitle={intl.formatMessage({
          id: 'pages.searchTable.title',
        })}
        formRef={formRef}
        actionRef={actionRef}
        rowKey={(record: API.FileInfo) => {
          return record.path;
        }}
        search={{
          labelWidth: "auto",
        }}
        columnsState={{
          value: columnsStateMap,
          onChange: setColumnsStateMap,
        }}
        toolBarRender={() => [
          <Button
            type="primary"
            key="primary"
            onClick={() => {
              // 转到欢迎页面
              history.push('/scan/file');
            }}
          >
            <SearchOutlined /> <FormattedMessage id="pages.searchTable.startSearch" />
          </Button>,
        ]}
        pagination={{
          pageSize: 200,
        }}
        request={async (
          // 第一个参数 params 查询表单和 params 参数的结合
          // 第一个参数中一定会有 pageSize 和  current ，这两个参数是 antd 的规范
          params: API.FileInfo & {
            pageSize?: number;
            current?: number;
            keywords?: string;
          },
          sort,
          filter,
        ) => {
          // 这里需要返回一个 Promise,在返回之前你可以进行数据转化
          // 如果需要转化参数可以在这里进行修改
          var list_param: API.listFilesParams = {
            path: params.path?params.path:"",
            page_no: params.current!,
            page_count: params.pageSize!,
          };
          console.info("sort", sort);
          console.info("filter", filter);

          const msg = await listFiles(list_param);
          return {
            data: msg.file_info_list,
            // success 请返回 true，
            // 不然 table 会停止解析数据，即使有数据
            success: true,
            // 不传会使用 data 的长度，如果是分页一定要传
            total: msg.total_count,
          };
        }}
        columns={columns}
        rowSelection={{
          onChange: (_, selectedRows) => {
            setSelectedRows(selectedRows);
          },
        }}
      />
      {selectedRowsState?.length > 0 && (
        <FooterToolbar
          extra={
            <div>
              <FormattedMessage id="pages.searchTable.chosen" />{' '}
              <a style={{ fontWeight: 600 }}>{selectedRowsState.length}</a>{' '}
              <FormattedMessage id="pages.searchTable.item" />
            </div>
          }
        >
          <Popconfirm
            title={intl.formatMessage({ id: "pages.searchTable.optionDeleteConfirmTitle" })}
            description={intl.formatMessage({ id: "pages.searchTable.optionDeleteConfirmDescription" })}
            onConfirm={async () => {
              await handleRemove(selectedRowsState, intl);
              setSelectedRows([]);
              actionRef.current?.reloadAndRest?.();
            }}
          >
            <Button>
              <FormattedMessage
                id="pages.searchTable.batchDeletion"
              />
            </Button>
          </Popconfirm>,

          {/** <Button type="primary">
            <FormattedMessage
              id="pages.searchTable.batchApproval"
            />
          </Button> */}


        </FooterToolbar>
      )}

      <Drawer
        width={600}
        open={showDetail}
        onClose={() => {
          setCurrentRow(undefined);
          setShowDetail(false);
        }}
        closable={false}
      >
        {currentRow?.path && (
          <ProDescriptions<API.FileInfo>
            column={2}
            title={currentRow?.name}
            request={async () => ({
              data: currentRow || {},
            })}
            params={{
              id: currentRow?.path,
            }}
            columns={columns as ProDescriptionsItemProps<API.FileInfo>[]}
          />
        )}
      </Drawer>
    </PageContainer>
  );
};

export default TableList;
