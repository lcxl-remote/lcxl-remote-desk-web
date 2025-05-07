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
const handleRemove = async (selectedRows: API.FileInfo[]) => {
  const hide = message.loading('正在删除');
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
    message.success('Deleted successfully and will refresh soon');
    return true;
  } catch (error) {
    hide();
    message.error('Delete failed, please try again');
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
          id="pages.searchTable.updateForm.ruleName.nameLabel"
        />
      ),
      dataIndex: ["name"],
      tooltip: '文件名称',
      render: (dom, entity) => {
        return (
          <a
            onClick={() => {
              setCurrentRow(entity);
              setShowDetail(true);
            }}
          >
            {dom}
          </a>
        );
      },
    },
    {
      key: 'file_extention_list',
      title: <FormattedMessage id="pages.searchTable.fileExtentionList" />,
      dataIndex: ["file_info", "file_extension"],
      hideInForm: true,
      hideInDescriptions: true,
      hideInTable: true,
      renderFormItem: (item, { type, defaultRender, ...rest }, form) => {
        const options: SelectProps['options'] = [];
        options.push({
          value: "jpg",
          label: "jpg",
        });
        options.push({
          value: "jpeg",
          label: "jpeg",
        });
        options.push({
          value: "bmp",
          label: "bmp",
        });
        options.push({
          value: "png",
          label: "png",
        });
        options.push({
          value: "heic",
          label: "heic",
        });
        options.push({
          value: "gif",
          label: "gif",
        });
        options.push({
          value: "avi",
          label: "avi",
        });
        options.push({
          value: "mp4",
          label: "mp4",
        });
        options.push({
          value: "mkv",
          label: "mkv",
        });
        return (
          <Select
            {...rest}
            mode="tags"
            //placeholder="Tags Mode"
            options={options}
          />
        );
      },
      //initialValue: ["jpg", "mp4"],
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
      key: 'search_file_created_time',
      title: (
        <FormattedMessage
          id="pages.searchTable.fileCreatedTime"
        />
      ),
      hideInTable: true,
      hideInDescriptions: true,
      dataIndex: ["created"],
      valueType: 'dateTimeRange',
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
      key: 'search_file_modified_time',
      title: (
        <FormattedMessage
          id="pages.searchTable.fileModifiedTime"
        />
      ),
      hideInTable: true,
      hideInDescriptions: true,
      dataIndex: ["modified"],
      valueType: 'dateTimeRange',
    },
    {
      key: 'search_file_size',
      title: (
        <FormattedMessage
          id="pages.searchTable.searchFileSize"
        />
      ),
      hideInTable: true,
      hideInDescriptions: true,
      dataIndex: ['file_info', "inode_info", "size"],
      valueType: 'digitRange',
      //initialValue: [2, null],
    },
    {
      key: 'search_md5_count',
      title: (
        <FormattedMessage
          id="pages.searchTable.searchMd5Count"
        />
      ),
      hideInTable: true,
      hideInDescriptions: true,
      dataIndex: ["md5_count"],
      valueType: 'digitRange',
      //initialValue: [2, null],
    },
    {
      title: <FormattedMessage id="pages.searchTable.titleOption" />,
      dataIndex: 'option',
      valueType: 'option',
      render: (_, record) => [
        <Popconfirm
          title={intl.formatMessage({ id: "pages.searchTable.optionDeleteConfirmTitle" })}
          description={intl.formatMessage({ id: "pages.searchTable.optionDeleteConfirmDescription" })}
          onConfirm={
            async () => {
              console.log("Begin to delete file: %s", record.path);
              try {
                const response = await deleteFile({
                  file_path: record.path,
                  delete_permanently: false,
                });
                console.log("Deleted file: %s: %s", record.path, response);
                setCurrentRow(undefined);
                actionRef.current?.reloadAndRest?.();
              } catch (err) {
                console.log("Request deleteFile error: " + err)
              }
            }
          }
        >
          <a key="config">
            <FormattedMessage id="pages.searchTable.deletion" />
          </a>
        </Popconfirm>,
        /*
        <a key="subscribeAlert" href="https://procomponents.ant.design/">
          <FormattedMessage
            id="pages.searchTable.subscribeAlert"
          />
        </a>,
        */
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
        actionRef={actionRef}
        rowKey={(record: API.FileInfo) => {
          return record.path;
        }}
        search={{
          labelWidth: 120,
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
        /*
        form={{
          request: async () => {
            const response = await queryListSettings();
            const form_params = response.data!;

            let search_file_modified_time: string[] | undefined = undefined;
            if (form_params.start_modified_time != null || form_params.end_modified_time != null) {
              search_file_modified_time = [];
              search_file_modified_time.push(form_params.start_modified_time);
              search_file_modified_time.push(form_params.end_modified_time);
            }

            let search_file_created_time: string[] | undefined = undefined;
            if (form_params.start_created_time != null || form_params.end_created_time != null) {
              search_file_created_time = [];
              search_file_created_time.push(form_params.start_created_time);
              search_file_created_time.push(form_params.end_created_time);
            }


            let search_md5_count: (number | undefined)[] = [];
            search_md5_count.push(form_params.min_md5_count);
            search_md5_count.push(form_params.max_md5_count);

            let file_extention_list: string[] | undefined = undefined;
            if (form_params.file_extension_list != null) {
              file_extention_list = form_params.file_extension_list.split(',');
            }

            let search_file_size: (number | undefined)[] = [];
            search_file_size.push(form_params.min_file_size);
            search_file_size.push(form_params.max_file_size);

            let file_info: API.FileInfo | undefined = undefined;
            file_info = {
              file_name: form_params.file_name,
              dir_path: form_params.dir_path
            };
            return {
              file_info,
              search_file_modified_time,
              search_file_created_time,
              search_md5_count,
              file_extention_list,
              search_file_size,
            };
          },
        }}
          */
        request={async (
          // 第一个参数 params 查询表单和 params 参数的结合
          // 第一个参数中一定会有 pageSize 和  current ，这两个参数是 antd 的规范
          params: API.FileInfo & {
            pageSize?: number;
            current?: number;
            keywords?: string;
            search_file_modified_time?: string[];
            search_file_created_time?: string[];
            file_extention_list?: string[];
            search_file_size?: number[];
          },
          sort,
          filter,
        ) => {
          // 这里需要返回一个 Promise,在返回之前你可以进行数据转化
          // 如果需要转化参数可以在这里进行修改
          var list_param: API.listFilesParams = {
            page_no: params.current!,
            page_count: params.pageSize!,
          };
          console.info("sort", sort);
          console.info("filter", filter);
          
        
          if (params.search_file_modified_time) {
            list_param.start_modified_time = new Date(params.search_file_modified_time[0]).toISOString();
            list_param.end_modified_time = new Date(params.search_file_modified_time[1]).toISOString();
          }
          if (params.search_file_created_time) {
            list_param.start_created_time = new Date(params.search_file_created_time[0]).toISOString();
            list_param.end_created_time = new Date(params.search_file_created_time[1]).toISOString();
          }
          
          if (params.file_extention_list && params.file_extention_list.length > 0) {
            list_param.file_extension_list = params.file_extention_list.join(',').toLowerCase();
          }
          if (params.search_file_size) {
            list_param.min_file_size = params.search_file_size[0];
            list_param.max_file_size = params.search_file_size[1];
          }

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
              await handleRemove(selectedRowsState);
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
            title={currentRow?.path}
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
